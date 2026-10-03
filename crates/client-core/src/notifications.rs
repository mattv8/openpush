use super::*;

type ExistingNotification = (
    String,
    String,
    bool,
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    bool,
);

pub(super) fn valid(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_NOTIFICATION_ID_BYTES
}
pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}
pub(super) fn valid_target(target: &NotificationTarget) -> bool {
    uuid::Uuid::parse_str(&target.source_device_id).is_ok()
        && valid(&target.notification_key)
        && uuid::Uuid::parse_str(&target.lifetime).is_ok()
}
fn unlocked(ctx: &Ctx<'_>) -> bool {
    ctx.active_epoch
        .is_some_and(|epoch| ctx.keys.contains_key(&epoch))
}
pub(super) fn valid_wire(n: &NotificationWire) -> bool {
    valid_target(&n.target)
        && valid(&n.instance)
        && valid(&n.package_name)
        && n.app_name.len() <= MAX_NOTIFICATION_ID_BYTES
        && n.title.len() + n.text.len() <= MAX_NOTIFICATION_TEXT_BYTES
        && (0..=253_402_300_799_999).contains(&n.posted_at)
        && n.category
            .as_ref()
            .is_none_or(|v| v.len() <= MAX_NOTIFICATION_ID_BYTES)
}

impl Client {
    pub fn notification_source_device_id(&self) -> Result<String, Error> {
        Ok(self.lock()?.config.device_id.to_string())
    }
    pub fn capture_notification(
        &self,
        input: NotificationCapture,
    ) -> Result<NotificationCaptureOutcome, Error> {
        if !valid(&input.notification_key)
            || !valid(&input.instance)
            || !valid(&input.package_name)
            || input.app_name.len() > MAX_NOTIFICATION_ID_BYTES
            || input.title.len() + input.text.len() > MAX_NOTIFICATION_TEXT_BYTES
            || !(0..=253_402_300_799_999).contains(&input.posted_at)
            || input
                .category
                .as_ref()
                .is_some_and(|v| v.len() > MAX_NOTIFICATION_ID_BYTES)
        {
            return Err(Error::InvalidRequest("notification"));
        }
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        // Notifications deliberately have no plaintext locked queue.
        if ctx
            .active_epoch
            .is_none_or(|epoch| !ctx.keys.contains_key(&epoch))
        {
            return Ok(NotificationCaptureOutcome::DroppedLocked);
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source = ctx.device_id.to_string();
        let muted: bool = tx
            .query_row(
                "SELECT muted FROM app_filters WHERE source_device_id=? AND package_name=?",
                params![source, input.package_name],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false);
        if muted {
            return Ok(NotificationCaptureOutcome::FilteredOut);
        }
        let existing: Option<ExistingNotification> = tx.query_row("SELECT lifetime,instance,removed,package_name,app_name,title,text,category,posted_at,dismissible FROM notifications WHERE source_device_id=? AND notification_key=?", params![ctx.device_id.to_string(), input.notification_key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?))).optional()?;
        let retained_post: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM notifications n JOIN outbox o ON o.seq=n.source_sequence WHERE n.source_device_id=? AND n.notification_key=?)", params![source,input.notification_key], |r| r.get(0))?;
        if retained_post
            && existing.as_ref().is_some_and(
                |(
                    _,
                    _instance,
                    removed,
                    package,
                    app,
                    title,
                    text,
                    category,
                    _posted_at,
                    dismissible,
                )| {
                    !removed
                        && package == &input.package_name
                        && app == &input.app_name
                        && title == &input.title
                        && text == &input.text
                        && category == &input.category
                        && *dismissible == input.dismissible
                },
            )
        {
            // Re-notifying unchanged content updates the OS identity, but does not create
            // another encrypted history record. Removal/dismissal still track the current OS row.
            tx.execute("UPDATE notifications SET instance=? WHERE source_device_id=? AND notification_key=?", params![input.instance, source, input.notification_key])?;
            tx.execute("UPDATE notification_dismissals SET instance=? WHERE source_device_id=? AND notification_key=? AND lifetime=(SELECT lifetime FROM notifications WHERE source_device_id=? AND notification_key=?) AND completed=0", params![input.instance, source,input.notification_key,source,input.notification_key])?;
            tx.commit()?;
            return Ok(NotificationCaptureOutcome::Duplicate);
        }
        let lifetime = if existing
            .as_ref()
            .is_some_and(|(_, _, removed, ..)| !removed)
        {
            existing.unwrap().0
        } else {
            uuid::Uuid::new_v4().to_string()
        };
        let wire = NotificationWire {
            target: NotificationTarget {
                source_device_id: ctx.device_id.to_string(),
                notification_key: input.notification_key.clone(),
                lifetime: lifetime.clone(),
            },
            instance: input.instance,
            package_name: input.package_name,
            app_name: input.app_name,
            title: input.title,
            text: input.text,
            category: input.category,
            posted_at: input.posted_at,
            dismissible: input.dismissible,
        };
        let (envelope_id, seq) = enqueue(
            &tx,
            &ctx,
            EnvelopePurpose::Event,
            None,
            None,
            &PrivatePayload::NotificationPosted {
                notification: wire.clone(),
            },
        )?;
        tx.execute(
            "INSERT INTO outbox_notification_posts(envelope_id,source_device_id,package_name) VALUES(?,?,?)",
            params![envelope_id.to_string(), wire.target.source_device_id, wire.package_name],
        )?;
        // Local captures belong to the phone, not its desktop banner queue.
        upsert_notification(&tx, &wire, seq, true)?;
        tx.commit()?;
        Ok(NotificationCaptureOutcome::Captured)
    }

    pub fn remove_notification(&self, notification_key: &str, instance: &str) -> Result<(), Error> {
        let mut guard = self.lock()?;
        let (conn, ctx) = guard.parts();
        if !valid(notification_key) || !valid(instance) {
            return Err(Error::InvalidRequest("notification"));
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String)> = tx.query_row("SELECT lifetime,instance FROM notifications WHERE source_device_id=? AND notification_key=? AND removed=0", params![ctx.device_id.to_string(),notification_key], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let Some((lifetime, _current_instance)) = row else {
            return Ok(());
        };
        // The Android host verifies that this key is no longer active before calling us.
        // Its removal may name an update coalesced (or dropped while locked) before capture.
        let target = NotificationTarget {
            source_device_id: ctx.device_id.to_string(),
            notification_key: notification_key.into(),
            lifetime,
        };
        let (_, seq) = enqueue(
            &tx,
            &ctx,
            EnvelopePurpose::Event,
            None,
            None,
            &PrivatePayload::NotificationRemoved {
                target: target.clone(),
                instance: instance.into(),
            },
        )?;
        remove_applied(&tx, &target, seq)?;
        Ok(tx.commit()?)
    }

    pub fn notification_snapshot(&self) -> Result<NotificationSnapshot, Error> {
        let s = self.lock()?;
        prune_banners(&s.conn)?;
        let mut q = s.conn.prepare("SELECT source_device_id,notification_key,lifetime,package_name,app_name,title,text,category,posted_at,dismissible,seen, EXISTS(SELECT 1 FROM notification_dismissals d WHERE d.source_device_id=notifications.source_device_id AND d.notification_key=notifications.notification_key AND d.lifetime=notifications.lifetime) FROM notifications WHERE removed=0 AND NOT EXISTS(SELECT 1 FROM app_filters f WHERE f.source_device_id=notifications.source_device_id AND f.package_name=notifications.package_name AND f.muted=1) ORDER BY posted_at DESC LIMIT 1000")?;
        let notifications = q
            .query_map([], |r| {
                Ok(MirroredNotification {
                    target: NotificationTarget {
                        source_device_id: r.get(0)?,
                        notification_key: r.get(1)?,
                        lifetime: r.get(2)?,
                    },
                    package_name: r.get(3)?,
                    app_name: r.get(4)?,
                    title: r.get(5)?,
                    text: r.get(6)?,
                    category: r.get(7)?,
                    posted_at: r.get(8)?,
                    dismissible: r.get(9)?,
                    seen: r.get(10)?,
                    dismissal_pending: r.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut q = s.conn.prepare("SELECT source_device_id,package_name,app_name,muted FROM app_filters ORDER BY source_device_id,package_name")?;
        let app_filters = q
            .query_map([], |r| {
                Ok(AppFilter {
                    source_device_id: r.get(0)?,
                    package_name: r.get(1)?,
                    app_name: r.get(2)?,
                    muted: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(NotificationSnapshot {
            notifications,
            app_filters,
        })
    }

    pub fn set_app_muted(
        &self,
        source_device_id: &str,
        package_name: &str,
        app_name: &str,
        muted: bool,
    ) -> Result<(), Error> {
        if uuid::Uuid::parse_str(source_device_id).is_err()
            || !valid(package_name)
            || app_name.len() > MAX_NOTIFICATION_ID_BYTES
        {
            return Err(Error::InvalidRequest("notification filter"));
        }
        let mut g = self.lock()?;
        let (conn, ctx) = g.parts();
        if !unlocked(&ctx) {
            return Err(Error::KeysUnavailable);
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let filter = AppFilter {
            source_device_id: source_device_id.into(),
            package_name: package_name.into(),
            app_name: app_name.into(),
            muted,
        };
        let revision = get_meta(&tx, "notification_filter_clock")?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(Error::Database)?;
        set_meta(&tx, "notification_filter_clock", &revision.to_string())?;
        let (_, sequence) = enqueue(
            &tx,
            &ctx,
            EnvelopePurpose::Event,
            None,
            None,
            &PrivatePayload::AppFilter {
                filter: filter.clone(),
                logical_revision: revision,
                writer_device_id: ctx.device_id.to_string(),
            },
        )?;
        apply_filter(
            &tx,
            &filter,
            revision,
            &ctx.device_id.to_string(),
            &ctx.device_id.to_string(),
            sequence,
        )?;
        if muted {
            tx.execute("DELETE FROM banner_candidates WHERE acknowledged=0 AND kind='notification' AND source_device_id=? AND notification_key IN (SELECT notification_key FROM notifications WHERE source_device_id=? AND package_name=?)",params![source_device_id,source_device_id,package_name])?;
        }
        Ok(tx.commit()?)
    }
    pub fn dismiss_notification(&self, target: NotificationTarget) -> Result<(), Error> {
        if !valid_target(&target) {
            return Err(Error::InvalidRequest("notification target"));
        }
        let mut g = self.lock()?;
        let (conn, ctx) = g.parts();
        if !unlocked(&ctx) {
            return Err(Error::KeysUnavailable);
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let clearable: Option<bool> = tx.query_row(
            "SELECT dismissible FROM notifications WHERE source_device_id=? AND notification_key=? AND lifetime=? AND removed=0",
            params![target.source_device_id,target.notification_key,target.lifetime], |r| r.get(0),
        ).optional()?;
        match clearable {
            Some(false) => return Err(Error::InvalidRequest("notification cannot be dismissed")),
            None => return Ok(()), // stale UI action cannot target a repost
            Some(true) => {}
        }
        if has_dismissal(&tx, &target)? {
            return Ok(());
        }
        enqueue(
            &tx,
            &ctx,
            EnvelopePurpose::Event,
            None,
            None,
            &PrivatePayload::NotificationDismiss {
                target: target.clone(),
            },
        )?;
        apply_dismiss(&tx, &target, false, ctx.active_epoch.unwrap())?;
        Ok(tx.commit()?)
    }
    pub fn mark_notifications_seen(&self, targets: Vec<NotificationTarget>) -> Result<(), Error> {
        if targets.len() > 1000 || targets.iter().any(|target| !valid_target(target)) {
            return Err(Error::InvalidRequest("notification targets"));
        }
        let mut s = self.lock()?;
        let tx = s
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for t in targets {
            tx.execute("UPDATE notifications SET seen=1 WHERE source_device_id=? AND notification_key=? AND lifetime=?",params![t.source_device_id,t.notification_key,t.lifetime])?;
        }
        prune_banners(&tx)?;
        tx.commit()?;
        Ok(())
    }
    pub fn pending_notification_dismissals(
        &self,
        limit: usize,
    ) -> Result<Vec<NotificationDismissal>, Error> {
        let s = self.lock()?;
        let Some(epoch) = s.active_epoch.filter(|epoch| s.keys.contains_key(epoch)) else {
            return Ok(Vec::new());
        };
        let mut q=s.conn.prepare("SELECT d.id,d.source_device_id,d.notification_key,d.lifetime,d.instance FROM notification_dismissals d JOIN notifications n ON n.source_device_id=d.source_device_id AND n.notification_key=d.notification_key AND n.lifetime=d.lifetime AND n.instance=d.instance WHERE d.completed=0 AND d.historical=0 AND d.source_device_id=? AND d.key_epoch=? AND n.removed=0 AND n.dismissible=1 ORDER BY d.rowid LIMIT ?")?;
        Ok(q.query_map(
            params![s.config.device_id.to_string(), epoch, limit.min(100) as i64],
            |r| {
                Ok(NotificationDismissal {
                    id: r.get(0)?,
                    target: NotificationTarget {
                        source_device_id: r.get(1)?,
                        notification_key: r.get(2)?,
                        lifetime: r.get(3)?,
                    },
                    instance: r.get(4)?,
                })
            },
        )?
        .collect::<Result<_, _>>()?)
    }
    pub fn complete_notification_dismissal(&self, id: &str) -> Result<(), Error> {
        self.lock()?.conn.execute(
            "UPDATE notification_dismissals SET completed=1 WHERE id=?",
            params![id],
        )?;
        Ok(())
    }
    pub fn pending_banner_candidates(&self, limit: usize) -> Result<Vec<BannerCandidate>, Error> {
        let s = self.lock()?;
        prune_banners(&s.conn)?;
        if s.active_epoch
            .is_none_or(|epoch| !s.keys.contains_key(&epoch))
        {
            return Ok(Vec::new());
        }
        let mut q=s.conn.prepare("SELECT id,kind,conversation_id,source_device_id,notification_key,lifetime,title,body,created_at FROM banner_candidates b WHERE acknowledged=0 AND ((kind='message' AND EXISTS(SELECT 1 FROM messages m WHERE m.id=b.id AND m.seen=0)) OR (kind='notification' AND EXISTS(SELECT 1 FROM notifications n WHERE n.source_device_id=b.source_device_id AND n.notification_key=b.notification_key AND n.lifetime=b.lifetime AND n.removed=0 AND NOT n.seen AND NOT EXISTS(SELECT 1 FROM notification_dismissals d WHERE d.source_device_id=n.source_device_id AND d.notification_key=n.notification_key AND d.lifetime=n.lifetime) AND NOT EXISTS(SELECT 1 FROM app_filters f WHERE f.source_device_id=n.source_device_id AND f.package_name=n.package_name AND f.muted=1)))) ORDER BY rowid LIMIT ?")?;
        Ok(q.query_map(params![limit.min(1000) as i64], |r| {
            Ok(BannerCandidate {
                id: r.get(0)?,
                kind: r.get(1)?,
                conversation_id: r.get(2)?,
                notification_target: match (
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ) {
                    (Some(a), Some(b), Some(c)) => Some(NotificationTarget {
                        source_device_id: a,
                        notification_key: b,
                        lifetime: c,
                    }),
                    _ => None,
                },
                title: r.get(6)?,
                body: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?
        .collect::<Result<_, _>>()?)
    }
    pub fn ack_banner_candidates(&self, ids: Vec<String>) -> Result<(), Error> {
        let s = self.lock()?;
        for id in ids {
            s.conn
                .execute("DELETE FROM banner_candidates WHERE id=?", params![id])?;
        }
        Ok(())
    }
}

fn upsert_notification(
    c: &Connection,
    n: &NotificationWire,
    seq: u64,
    historical: bool,
) -> Result<(), Error> {
    let tombstone: Option<i64> = c.query_row("SELECT source_sequence FROM notification_tombstones WHERE source_device_id=? AND notification_key=? AND lifetime=?", params![n.target.source_device_id,n.target.notification_key,n.target.lifetime], |r| r.get(0)).optional()?;
    // Removal ends a lifetime permanently. A fresh post must carry a fresh lifetime.
    if tombstone.is_some() {
        return Ok(());
    }
    let old:Option<(String,i64)>=c.query_row("SELECT lifetime,source_sequence FROM notifications WHERE source_device_id=? AND notification_key=?",params![n.target.source_device_id,n.target.notification_key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let first_post = old
        .as_ref()
        .is_none_or(|(lifetime, _)| lifetime != &n.target.lifetime);
    if old.as_ref().is_some_and(|(_, s)| *s >= seq as i64) {
        return Ok(());
    }
    c.execute("INSERT INTO notifications(source_device_id,notification_key,lifetime,instance,package_name,app_name,title,text,category,posted_at,dismissible,seen,removed,source_sequence) VALUES(?,?,?,?,?,?,?,?,?,?,?,0,0,?) ON CONFLICT(source_device_id,notification_key) DO UPDATE SET seen=CASE WHEN notifications.lifetime=excluded.lifetime THEN notifications.seen ELSE 0 END,lifetime=excluded.lifetime,instance=excluded.instance,package_name=excluded.package_name,app_name=excluded.app_name,title=excluded.title,text=excluded.text,category=excluded.category,posted_at=excluded.posted_at,dismissible=excluded.dismissible,removed=0,source_sequence=excluded.source_sequence WHERE excluded.source_sequence>notifications.source_sequence",params![n.target.source_device_id,n.target.notification_key,n.target.lifetime,n.instance,n.package_name,n.app_name,n.title,n.text,n.category,n.posted_at,n.dismissible,seq as i64])?;
    // A dismissal received before the post remains presentation state; bind its eventual
    // OS effect to the first matching instance only, never to a subsequent update.
    c.execute("UPDATE notification_dismissals SET instance=? WHERE source_device_id=? AND notification_key=? AND lifetime=? AND completed=0", params![n.instance,n.target.source_device_id,n.target.notification_key,n.target.lifetime])?;
    c.execute("INSERT OR IGNORE INTO app_filters(source_device_id,package_name,app_name,muted) VALUES(?,?,?,0)", params![n.target.source_device_id,n.package_name,n.app_name])?;
    if !historical && first_post {
        let id = json(&n.target)?;
        c.execute("INSERT OR IGNORE INTO banner_candidates(id,kind,conversation_id,source_device_id,notification_key,lifetime,title,body,created_at) VALUES(?, 'notification',NULL,?,?,?,?,?,?)",params![id,n.target.source_device_id,n.target.notification_key,n.target.lifetime,if n.title.is_empty() { &n.app_name } else { &n.title },n.text,now_ms()])?;
    }
    prune_banners(c)?;
    Ok(())
}
fn remove_applied(c: &Connection, t: &NotificationTarget, seq: u64) -> Result<(), Error> {
    c.execute("INSERT INTO notification_tombstones(source_device_id,notification_key,lifetime,source_sequence) VALUES(?,?,?,?) ON CONFLICT(source_device_id,notification_key,lifetime) DO UPDATE SET source_sequence=MAX(source_sequence,excluded.source_sequence)",params![t.source_device_id,t.notification_key,t.lifetime,seq as i64])?;
    c.execute("UPDATE notifications SET removed=1,title='',text='',source_sequence=? WHERE source_device_id=? AND notification_key=? AND lifetime=? AND source_sequence<=?",params![seq as i64,t.source_device_id,t.notification_key,t.lifetime,seq as i64])?;
    c.execute("DELETE FROM banner_candidates WHERE source_device_id=? AND notification_key=? AND lifetime=?", params![t.source_device_id,t.notification_key,t.lifetime])?;
    c.execute("UPDATE notification_dismissals SET completed=1 WHERE source_device_id=? AND notification_key=? AND lifetime=?", params![t.source_device_id,t.notification_key,t.lifetime])?;
    Ok(())
}
pub(super) fn apply_filter(
    c: &Connection,
    f: &AppFilter,
    revision: u64,
    writer: &str,
    producer: &str,
    sequence: u64,
) -> Result<(), Error> {
    let revision = i64::try_from(revision).map_err(|_| Error::InvalidRequest("filter revision"))?;
    let clock = get_meta(c, "notification_filter_clock")?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    set_meta(
        c,
        "notification_filter_clock",
        &clock.max(revision).to_string(),
    )?;
    let changed = c.execute("INSERT INTO app_filters(source_device_id,package_name,app_name,muted,source_sequence,producer_device_id,logical_revision,writer_device_id) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(source_device_id,package_name) DO UPDATE SET app_name=excluded.app_name,muted=excluded.muted,source_sequence=excluded.source_sequence,producer_device_id=excluded.producer_device_id,logical_revision=excluded.logical_revision,writer_device_id=excluded.writer_device_id WHERE excluded.logical_revision>app_filters.logical_revision OR (excluded.logical_revision=app_filters.logical_revision AND excluded.writer_device_id>app_filters.writer_device_id)",params![f.source_device_id,f.package_name,f.app_name,f.muted,sequence as i64,producer,revision,writer])?;
    if changed > 0 && f.muted {
        // Sealed bytes cannot be edited. Remove entire unsent post envelopes using metadata,
        // without interpreting ciphertext or disturbing unrelated SMS/command outbox rows.
        c.execute("DELETE FROM outbox_conflicts WHERE envelope_id IN (SELECT p.envelope_id FROM outbox_notification_posts p JOIN outbox o ON o.envelope_id=p.envelope_id WHERE p.source_device_id=? AND p.package_name=? AND o.state!='acknowledged')", params![f.source_device_id,f.package_name])?;
        // Purged posts will never upload: drop them from the compaction frontier so no later
        // record references a sequence the server can never resolve.
        c.execute("DELETE FROM compaction_frontier WHERE producer_device_id=(SELECT v FROM metadata WHERE k='device_id') AND source_sequence IN (SELECT o.seq FROM outbox o JOIN outbox_notification_posts p ON p.envelope_id=o.envelope_id WHERE o.state!='acknowledged' AND p.source_device_id=? AND p.package_name=?)", params![f.source_device_id,f.package_name])?;
        c.execute("DELETE FROM outbox WHERE state!='acknowledged' AND envelope_id IN (SELECT envelope_id FROM outbox_notification_posts WHERE source_device_id=? AND package_name=?)", params![f.source_device_id,f.package_name])?;
        c.execute("DELETE FROM banner_candidates WHERE kind='notification' AND source_device_id=? AND notification_key IN (SELECT notification_key FROM notifications WHERE source_device_id=? AND package_name=?)",params![f.source_device_id,f.source_device_id,f.package_name])?;
    }
    Ok(())
}

pub(super) fn prune_banners(c: &Connection) -> Result<(), Error> {
    c.execute("DELETE FROM banner_candidates WHERE created_at<? OR (kind='message' AND NOT EXISTS(SELECT 1 FROM messages m WHERE m.id=banner_candidates.id AND m.seen=0)) OR (kind='notification' AND NOT EXISTS(SELECT 1 FROM notifications n WHERE n.source_device_id=banner_candidates.source_device_id AND n.notification_key=banner_candidates.notification_key AND n.lifetime=banner_candidates.lifetime AND n.removed=0 AND n.seen=0 AND NOT EXISTS(SELECT 1 FROM notification_dismissals d WHERE d.source_device_id=n.source_device_id AND d.notification_key=n.notification_key AND d.lifetime=n.lifetime)))", params![now_ms().saturating_sub(86_400_000)])?;
    Ok(())
}
fn has_dismissal(c: &Connection, t: &NotificationTarget) -> Result<bool, Error> {
    Ok(c.query_row("SELECT 1 FROM notification_dismissals WHERE source_device_id=? AND notification_key=? AND lifetime=? LIMIT 1",params![t.source_device_id,t.notification_key,t.lifetime], |_| Ok(())).optional()?.is_some())
}
fn apply_dismiss(
    c: &Connection,
    t: &NotificationTarget,
    historical: bool,
    epoch: u32,
) -> Result<(), Error> {
    if has_dismissal(c, t)? {
        return Ok(());
    }
    let instance:Option<String>=c.query_row("SELECT instance FROM notifications WHERE source_device_id=? AND notification_key=? AND lifetime=? AND removed=0 AND dismissible=1",params![t.source_device_id,t.notification_key,t.lifetime],|r|r.get(0)).optional()?;
    c.execute("INSERT OR IGNORE INTO notification_dismissals(id,source_device_id,notification_key,lifetime,instance,historical,key_epoch) VALUES(?,?,?,?,?,?,?)",params![json(t)?,t.source_device_id,t.notification_key,t.lifetime,instance.unwrap_or_default(),historical,epoch])?;
    Ok(())
}
/// Shape and authorship rules for a received `AppFilter` event.
pub(super) fn valid_filter_event(
    filter: &AppFilter,
    logical_revision: u64,
    writer_device_id: &str,
    producer: &str,
) -> bool {
    valid(&filter.source_device_id)
        && valid(&filter.package_name)
        && uuid::Uuid::parse_str(&filter.source_device_id).is_ok()
        && uuid::Uuid::parse_str(writer_device_id).is_ok()
        && writer_device_id == producer
        && filter.app_name.len() <= MAX_NOTIFICATION_ID_BYTES
        && logical_revision != 0
        && logical_revision < i64::MAX as u64
}
pub(super) fn apply_notification_payload(
    c: &Connection,
    p: PrivatePayload,
    producer: DeviceId,
    seq: u64,
    historical: bool,
    epoch: u32,
) -> Result<bool, Error> {
    if i64::try_from(seq).is_err() {
        return Ok(false);
    }
    match p {
        PrivatePayload::NotificationPosted { notification } => {
            if !valid_wire(&notification)
                || notification.target.source_device_id != producer.to_string()
            {
                return Ok(false);
            };
            upsert_notification(c, &notification, seq, historical)?;
            Ok(true)
        }
        PrivatePayload::NotificationRemoved { target, instance } => {
            if !valid_target(&target)
                || !valid(&instance)
                || target.source_device_id != producer.to_string()
            {
                return Ok(false);
            };
            remove_applied(c, &target, seq)?;
            Ok(true)
        }
        PrivatePayload::NotificationDismiss { target } => {
            if !valid_target(&target) {
                return Ok(false);
            }
            apply_dismiss(c, &target, historical, epoch)?;
            Ok(true)
        }
        PrivatePayload::AppFilter {
            filter,
            logical_revision,
            writer_device_id,
        } => {
            if !valid_filter_event(
                &filter,
                logical_revision,
                &writer_device_id,
                &producer.to_string(),
            ) {
                return Ok(false);
            };
            apply_filter(
                c,
                &filter,
                logical_revision,
                &writer_device_id,
                &producer.to_string(),
                seq,
            )?;
            Ok(true)
        }
        _ => Ok(false),
    }
}
