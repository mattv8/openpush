import { useEffect, useRef, useState } from "react";
import QRCode from "qrcode";

export type PairingIntent = {
  httpsOrigin: string;
  intentToken: string;
  expiresInSeconds: number;
};

export type PairingStatus = {
  claimed: boolean;
  approved: boolean;
  deviceId?: string;
  keyDigest?: string;
  sas?: string;
  expiresInSeconds: number;
};

type State = "idle" | "pending" | "claimed" | "approved" | "expired" | "error";
const POLL_MS = 3_000;

/** The exact cross-host QR payload consumed by Android and iOS. */
export const pairingQrPayload = (intent: PairingIntent) => JSON.stringify({
  https_origin: intent.httpsOrigin,
  intent_token: intent.intentToken,
});

const validDigest = (value: string | undefined) => Boolean(value && /^[0-9a-f]{64}$/.test(value));
const validSas = (value: string | undefined) => Boolean(value && /^\d{6}$/.test(value));

/** A bounded, display-only owner approval panel. Credentials stay in the native host. */
export function PairPhone({
  createIntent,
  getStatus,
  approveIntent,
}: {
  createIntent(): Promise<PairingIntent>;
  getStatus(intentToken: string): Promise<PairingStatus>;
  approveIntent(intentToken: string, keyDigest: string): Promise<void>;
}) {
  const [state, setState] = useState<State>("idle");
  const [intent, setIntent] = useState<PairingIntent>();
  const [status, setStatus] = useState<PairingStatus>();
  const [qr, setQr] = useState("");
  const [confirmed, setConfirmed] = useState(false);
  const [error, setError] = useState("");
  const task = useRef(0);
  const timer = useRef<number | undefined>(undefined);

  const stopPolling = () => {
    task.current += 1;
    if (timer.current !== undefined) window.clearTimeout(timer.current);
    timer.current = undefined;
  };
  useEffect(() => () => { stopPolling(); }, []);
  useEffect(() => {
    if (!intent) return;
    const current = ++task.current;
    const expiresAt = Date.now() + Math.max(0, intent.expiresInSeconds) * 1_000;
    const poll = async () => {
      try {
        const next = await getStatus(intent.intentToken);
        if (task.current !== current) return;
        setStatus(next);
        const remaining = Math.min(next.expiresInSeconds * 1_000, expiresAt - Date.now());
        if (remaining <= 0) return setState("expired");
        if (next.approved) return setState("approved");
        if (next.claimed) setState("claimed");
        timer.current = window.setTimeout(poll, Math.min(POLL_MS, remaining));
      } catch (cause) {
        if (task.current === current) { setState("error"); setError(cause instanceof Error ? cause.message : "Could not check pairing status."); }
      }
    };
    void poll();
    return () => { stopPolling(); };
  }, [getStatus, intent]);

  const begin = async () => {
    stopPolling();
    setState("idle"); setIntent(undefined); setStatus(undefined); setQr(""); setConfirmed(false); setError("");
    try {
      const next = await createIntent();
      const content = pairingQrPayload(next);
      const image = await QRCode.toDataURL(content, { errorCorrectionLevel: "M", margin: 1, width: 256 });
      setIntent(next); setQr(image); setState("pending");
    } catch (cause) {
      setState("error");
      setError(cause instanceof Error ? cause.message : "Could not create a phone pairing request.");
    }
  };
  const approve = async () => {
    const claimedStatus = status;
    const displayedDigest = claimedStatus?.keyDigest;
    const displayedSas = claimedStatus?.sas;
    if (!intent || typeof displayedDigest !== "string" || typeof displayedSas !== "string" || !validDigest(displayedDigest) || !validSas(displayedSas) || !confirmed) return;
    try {
      await approveIntent(intent.intentToken, displayedDigest);
      setState("approved");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Could not approve this phone.");
    }
  };
  const cancel = () => {
    stopPolling();
    setState("idle"); setIntent(undefined); setStatus(undefined); setQr(""); setConfirmed(false); setError("");
  };

  return <section id="pairing-panel" aria-label="Pair phone" data-pairing-state={state}>
    <h2>Pair phone</h2>
    <p>Generate a short-lived QR code, then scan it from the Peppy phone app.</p>
    {(state === "idle" || state === "error" || state === "expired") && <button type="button" className="secondary-button" data-testid="pairing-new-qr-button" onClick={() => void begin()}>Generate QR code</button>}
    {qr && <img data-testid="pairing-qr-image" src={qr} alt="QR code for pairing a phone" />}
    {state === "pending" && <p data-testid="pairing-waiting" role="status">Waiting for phone scan…</p>}
    {state === "claimed" && <div data-testid="pairing-approve-prompt">
      <p>Phone scanned — compare this verification code on the phone before approving.</p>
      <output data-testid="pairing-sas-code" aria-label={`Verification code: ${status?.sas ?? ""}`}>{status?.sas}</output>
      <label><input type="checkbox" checked={confirmed} onChange={event => setConfirmed(event.target.checked)} /> I compared the verification code on the phone.</label>
      <button type="button" className="primary-button" disabled={!confirmed || !validDigest(status?.keyDigest) || !validSas(status?.sas)} onClick={() => void approve()}>Approve pairing</button>
    </div>}
    {(state === "pending" || state === "claimed") && <button type="button" className="secondary-button" onClick={cancel}>Cancel pairing</button>}
    {state === "approved" && <p data-testid="pairing-success" role="status">Phone approved — finish pairing on the phone.</p>}
    {state === "expired" && <p data-testid="pairing-expired" role="alert">QR code expired — generate a new one.</p>}
    {error && <p role="alert">{error}</p>}
  </section>;
}
