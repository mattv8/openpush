import { useEffect, useRef, useState, type ReactNode } from "react";
import { bridge, type HostedPreviewView } from "./bridge";
import { peppyCopy, type PeppyCopyKey } from "./generated/peppyCopy";
import "./hosted-onboarding.css";

const copy = (key: string) => peppyCopy[key as PeppyCopyKey] ?? peppyCopy.preview_error;

const steps: Record<string, number> = {
  welcome: 0,
  signin: 1,
  subscribe: 2,
  subscription_verifying: 2,
  purchase_pending: 2,
  passphrase: 3,
  confirm: 3,
  provisioning: 4,
  join: 5,
  approval: 5,
  unlock: 6,
  permissions: 7,
  settings: 8,
  delete_account: 8,
  lapsed: 8,
};

const entitlementCopy = (state: string) => copy({
  active: "settings_server_active",
  grace: "settings_server_grace",
  billing_retry: "settings_server_retry",
  expired: "settings_server_expired",
  revoked: "settings_server_revoked",
}[state] ?? "settings_server_none");

function ScreenSection({ id, children }: { id: string; children: ReactNode }) {
  return (
    <section id={id} aria-labelledby="hosted-screen-heading">
      {children}
    </section>
  );
}

function ApprovalSheet({
  code,
  onClose,
  onAllow,
  onDeny,
}: {
  code: string;
  onClose(): void;
  onAllow(): void;
  onDeny(): void;
}) {
  const heading = useRef<HTMLHeadingElement>(null);

  useEffect(() => {
    heading.current?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onClose();
        return;
      }
      if (event.key !== "Tab") return;
      const sheet = document.getElementById("device-approval-sheet");
      const focusable = sheet
        ? [...sheet.querySelectorAll<HTMLElement>("button, [tabindex]:not([tabindex='-1'])")]
        : [];
      const current = focusable.indexOf(document.activeElement as HTMLElement);
      if (!focusable.length) return;
      if (event.shiftKey && current <= 0) {
        event.preventDefault();
        focusable.at(-1)?.focus();
      } else if (!event.shiftKey && current === focusable.length - 1) {
        event.preventDefault();
        focusable[0]?.focus();
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  return (
    <div
      id="device-approval-sheet"
      role="dialog"
      aria-modal="true"
      aria-labelledby="approval-sheet-heading"
      onMouseDown={event => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="hosted-approval-content">
        <h3 id="approval-sheet-heading" ref={heading} tabIndex={-1}>
          {copy("device_approval_headline")}
        </h3>
        <p>{copy("device_approval_body")}</p>
        <code className="hosted-approval-code">{code}</code>
        <button id="device-allow-button" className="primary-button" onClick={onAllow}>
          {copy("device_allow")}
        </button>
        <button id="device-deny-button" className="secondary-button hosted-destructive" onClick={onDeny}>
          {copy("device_deny")}
        </button>
      </div>
    </div>
  );
}

export function HostedOnboarding({ onSelfHosted }: { onSelfHosted(): void }) {
  const [view, setView] = useState<HostedPreviewView | null>(null);
  const [busy, setBusy] = useState(false);
  const [bridgeError, setBridgeError] = useState(false);
  const [acknowledged, setAcknowledged] = useState(false);
  const [notifications, setNotifications] = useState(false);
  const [loginAtLaunch, setLoginAtLaunch] = useState(false);
  const [approvalOpen, setApprovalOpen] = useState(false);
  const [showManagePreview, setShowManagePreview] = useState(false);
  const heading = useRef<HTMLHeadingElement>(null);
  const observedEntry = useRef<string | null>(null);
  const approvalTrigger = useRef<HTMLElement | null>(null);

  const update = async (request: () => Promise<HostedPreviewView>) => {
    setBusy(true);
    setBridgeError(false);
    try {
      setView(await request());
    } catch {
      setBridgeError(true);
    } finally {
      setBusy(false);
    }
  };

  const advance = (event: string) => void update(() => bridge.hosted_preview_advance(event));
  const focusScreenHeading = () => requestAnimationFrame(() => heading.current?.focus());

  useEffect(() => {
    void update(() => bridge.hosted_preview_state());
  }, []);

  useEffect(() => {
    heading.current?.focus();
  }, [view?.screen]);

  useEffect(() => {
    if (!view) return;
    const entry = `${view.screen}:${view.statusKey ?? ""}`;
    const entryChanged = observedEntry.current !== entry;
    observedEntry.current = entry;
    if (!entryChanged || view.rejected) return;
    if (view.screen === "subscription_verifying") {
      void update(() => bridge.hosted_preview_advance("verify_entitlement"));
    } else if (view.screen === "provisioning" && view.statusKey === null) {
      void update(() => bridge.hosted_preview_advance("provision_finished"));
    }
  }, [view?.screen, view?.statusKey, view?.rejected]);

  if (!view) return <section id="hosted-onboarding" data-screen="loading" />;

  const code = view.fixture.approvalCode ?? "418 207";
  const title = (key: string) => (
    <h2 id="hosted-screen-heading" ref={heading} tabIndex={-1}>
      {copy(key)}
    </h2>
  );
  const back = () => (
    <button className="secondary-button" disabled={busy} onClick={() => advance("back")}>
      {copy("back")}
    </button>
  );
  const closeApproval = () => {
    setApprovalOpen(false);
    if (view.screen === "approval") advance("back");
    focusScreenHeading();
  };
  const approve = (event: "approval_granted" | "approval_denied") => {
    setApprovalOpen(false);
    if (view.screen !== "settings") advance(event);
    focusScreenHeading();
  };
  const openApproval = () => {
    approvalTrigger.current = document.getElementById("preview-approval-button");
    setApprovalOpen(true);
    if (view.screen === "join") advance("show_approval");
  };

  let content: ReactNode;
  if (view.screen === "welcome") {
    content = (
      <ScreenSection id="onboarding-screen">
        {title("onboarding_headline")}
        <img src="/peppy-logo.svg" alt="" />
        <p>{copy("onboarding_body")}</p>
        <button id="onboarding-hosted-cta" className="primary-button" disabled={busy} onClick={() => advance("hosted_start")}>
          {copy("onboarding_hosted_cta")}
        </button>
        <p className="setup-hint">{copy("onboarding_hosted_sub")}</p>
        <button id="onboarding-self-hosted-cta" className="secondary-button" disabled={busy} onClick={onSelfHosted}>
          {copy("onboarding_self_hosted_cta")}
        </button>
      </ScreenSection>
    );
  } else if (view.screen === "signin") {
    content = (
      <ScreenSection id="hosted-signin-screen">
        {title("hosted_sign_in_headline")}
        <p>{copy("hosted_sign_in_body")}</p>
        <button id="hosted-signin-apple" className="primary-button" disabled={busy} onClick={() => advance("signed_in")}>
          {copy("hosted_sign_in_apple")}
        </button>
        <button id="hosted-signin-google" className="primary-button" disabled={busy} onClick={() => advance("signed_in")}>
          {copy("hosted_sign_in_google")}
        </button>
        {back()}
      </ScreenSection>
    );
  } else if (["subscribe", "subscription_verifying", "purchase_pending"].includes(view.screen)) {
    const subscription = view.fixture.subscription;
    const status = subscription ? entitlementCopy(subscription.status) : copy("settings_server_none");
    content = (
      <ScreenSection id="subscribe-screen">
        {title("hosted_subscribe_headline")}
        {subscription ? <p>{subscription.displayPrice}</p> : <p>{copy("hosted_subscribe_store_unavailable")}</p>}
        {subscription && status !== copy("settings_server_none") && <p>{status}</p>}
        {view.screen === "subscription_verifying" ? (
          <span role="status" aria-label={copy("hosted_purchase_verifying")}>{copy("hosted_purchase_verifying")}</span>
        ) : view.screen === "purchase_pending" ? (
          <>
            <p>{copy("hosted_purchase_pending_body")}</p>
            <button id="hosted-restore-cta" className="primary-button" disabled={busy} onClick={() => advance("restore_succeeded")}>
              {copy("hosted_subscribe_restore")}
            </button>
          </>
        ) : subscription ? (
          <>
            <button id="hosted-subscribe-cta" className="primary-button" disabled={busy} onClick={() => advance("purchase_succeeded")}>
              {copy("hosted_subscribe_cta")}
            </button>
            <button id="hosted-restore-cta" className="secondary-button" disabled={busy} onClick={() => advance("restore_succeeded")}>
              {copy("hosted_subscribe_restore")}
            </button>
          </>
        ) : (
          <button id="hosted-store-retry" className="primary-button" disabled={busy} onClick={() => advance("store_retry")}>
            {copy("try_again")}
          </button>
        )}
        <p className="setup-hint">{copy("hosted_subscribe_legal")}</p>
        {back()}
      </ScreenSection>
    );
  } else if (["passphrase", "confirm"].includes(view.screen)) {
    content = (
      <ScreenSection id="passphrase-create-screen">
        {title("passphrase_create_headline")}
        <p>{copy("passphrase_create_body")}</p>
        <p className="setup-hint hosted-warning">{copy("passphrase_irrecoverable_warn")}</p>
        <p className="setup-hint">{copy("passphrase_native_note")}</p>
        <label className="settings-check">
          <input id="passphrase-ack" type="checkbox" checked={acknowledged} disabled={busy} onChange={event => setAcknowledged(event.target.checked)} />
          {copy("passphrase_ack_label")}
        </label>
        <button id="passphrase-native-cta" className="primary-button" disabled={!acknowledged || busy} onClick={() => void update(() => bridge.hosted_preview_create_passphrase())}>
          {copy("passphrase_native_cta")}
        </button>
        {busy && <p className="setup-hint">{copy("hosted_preview_native_only")}</p>}
        {view.localError && <p role="alert" className="settings-error">{copy(view.localError)}</p>}
        {back()}
      </ScreenSection>
    );
  } else if (view.screen === "provisioning") {
    content = (
      <ScreenSection id="provisioning-screen">
        {title("provisioning_headline")}
        {view.statusKey ? (
          <button id="provisioning-retry" className="primary-button" disabled={busy} onClick={() => advance("provision_retry")}>
            {copy("try_again")}
          </button>
        ) : (
          <div className="hosted-spinner" role="progressbar" aria-label={copy("provisioning_headline")} aria-valuemin={0} aria-valuemax={1} />
        )}
      </ScreenSection>
    );
  } else if (["join", "approval"].includes(view.screen)) {
    content = (
      <ScreenSection id="hosted-join-screen">
        {title("hosted_join_headline")}
        <p>{copy("hosted_join_body")}</p>
        <code className="hosted-approval-code" aria-label={`Approval code: ${code}`}>{code}</code>
        <button id="preview-approval-button" className="primary-button" disabled={busy} onClick={openApproval}>{copy("preview_approval")}</button>
        <button id="hosted-join-fallback" className="secondary-button" disabled={busy} onClick={() => advance("passphrase_fallback")}>{copy("hosted_join_fallback")}</button>
        {view.statusKey && <p>{copy(view.statusKey)}</p>}
        {back()}
      </ScreenSection>
    );
  } else if (view.screen === "unlock") {
    content = (
      <ScreenSection id="hosted-unlock-screen">
        {title("hosted_unlock_headline")}
        <p>{copy("hosted_unlock_body")}</p>
        <button id="hosted-unlock-button" className="primary-button" disabled={busy} onClick={() => void update(() => bridge.hosted_preview_unlock())}>
          {copy("hosted_unlock_cta")}
        </button>
        {back()}
      </ScreenSection>
    );
  } else if (view.screen === "permissions") {
    content = (
      <ScreenSection id="permissions-screen">
        {title("permissions_headline")}
        <p>{copy("permissions_body")}</p>
        <label className="settings-check">
          <input id="permissions-notifications" type="checkbox" checked={notifications} disabled={busy} onChange={event => setNotifications(event.target.checked)} />
          {copy("permissions_notifications")}
        </label>
        <label className="settings-check">
          <input id="permissions-login" type="checkbox" checked={loginAtLaunch} disabled={busy} onChange={event => setLoginAtLaunch(event.target.checked)} />
          {copy("permissions_login")}
        </label>
        <p className="setup-hint">{copy("permissions_desktop_note")}</p>
        <button id="permissions-done" className="primary-button" disabled={busy} onClick={() => advance("permissions_done")}>
          {copy("permissions_done")}
        </button>
        <button id="permissions-skip" className="secondary-button" disabled={busy} onClick={() => advance("permissions_skipped")}>
          {copy("permissions_skip")}
        </button>
      </ScreenSection>
    );
  } else if (view.screen === "lapsed") {
    content = (
      <ScreenSection id="hosted-lapsed-screen">
        {title("hosted_lapsed_headline")}
        <p>{copy("hosted_lapsed_body")}</p>
        <button id="hosted-lapsed-resubscribe" className="primary-button" disabled={busy} onClick={() => advance("resubscribe")}>
          {copy("hosted_lapsed_resubscribe")}
        </button>
        <button id="hosted-lapsed-sign-out" className="secondary-button" disabled={busy} onClick={() => advance("signout")}>
          {copy("settings_server_sign_out")}
        </button>
        {back()}
      </ScreenSection>
    );
  } else if (view.screen === "delete_account") {
    content = (
      <ScreenSection id="settings-server-section">
        {title("settings_server_delete_title")}
        <p>{copy("settings_server_delete_body")}</p>
        <button id="delete-account-confirm" className="primary-button hosted-destructive" disabled={busy} onClick={() => advance("deletion_confirmed")}>
          {copy("settings_server_delete")}
        </button>
        <button id="delete-account-cancel" className="secondary-button" disabled={busy} onClick={() => advance("cancel")}>
          {copy("cancel")}
        </button>
      </ScreenSection>
    );
  } else {
    content = (
      <ScreenSection id="settings-server-section">
        {title("preview_complete")}
        <h3>{copy("settings_server_section")}</h3>
        <dl id="hosted-settings-rows">
          <div>
            <dt>{copy("settings_server_origin")}</dt>
            <dd>{view.fixture.hostedOrigin ?? "—"}</dd>
          </div>
          <div>
            <dt>{copy("settings_server_status")}</dt>
            <dd>{entitlementCopy(view.entitlementState)}</dd>
          </div>
          <div>
            <dt>{copy("settings_server_account")}</dt>
            <dd>{view.fixture.accountLabel ?? "—"}</dd>
          </div>
        </dl>
        <button id="settings-manage" className="primary-button" disabled={busy} onClick={() => setShowManagePreview(true)}>
          {copy("settings_server_manage")}
        </button>
        {showManagePreview && <p className="setup-hint" aria-live="polite">{copy("settings_server_manage_preview")}</p>}
        {["expired", "revoked"].includes(view.entitlementState) && (
          <button id="settings-resubscribe" className="secondary-button" disabled={busy} onClick={() => advance("resubscribe")}>
            {copy("hosted_lapsed_resubscribe")}
          </button>
        )}
        <button id="settings-sign-out" className="secondary-button" disabled={busy} onClick={() => advance("signout")}>
          {copy("settings_server_sign_out")}
        </button>
        <button id="settings-delete" className="secondary-button hosted-destructive" disabled={busy} onClick={() => advance("delete_account")}>
          {copy("settings_server_delete")}
        </button>
        <button id="preview-approval-button" className="secondary-button" disabled={busy} onClick={openApproval}>
          {copy("preview_approval")}
        </button>
        {back()}
      </ScreenSection>
    );
  }

  return (
    <section id="hosted-onboarding" data-screen={view.screen}>
      <div className="hosted-onboarding-card">
        <header id="hosted-preview-banner">
          <span>{copy("preview_label")}</span>
          <select
            id="preview-scenario-picker"
            aria-label={copy("preview_scenarios")}
            disabled={busy}
            value={view.scenario}
            onChange={event => void update(() => bridge.hosted_preview_start(event.target.value))}
          >
            {view.scenarios.map(scenario => <option key={scenario}>{scenario}</option>)}
          </select>
          <button id="preview-reset-button" className="secondary-button" disabled={busy} onClick={() => void update(() => bridge.hosted_preview_reset())}>
            {copy("preview_reset")}
          </button>
        </header>
        <div id="hosted-onboarding-progress" aria-hidden="true" data-step={steps[view.screen] ?? 0} />
        {content}
        <div id="hosted-status-region" role="status" aria-live="polite" aria-atomic="true" className="visually-hidden">
          {view.statusKey ? copy(view.statusKey) : ""}
        </div>
        {bridgeError && <p role="alert" className="settings-error">{copy("preview_error")}</p>}
      </div>
      {approvalOpen && (
        <ApprovalSheet
          code={code}
          onClose={closeApproval}
          onAllow={() => approve("approval_granted")}
          onDeny={() => approve("approval_denied")}
        />
      )}
    </section>
  );
}
