"use client";

import { useCallback, useEffect, useState } from "react";
import { api, ApiError, describeError } from "@/lib/api";
import { FINAL_STATES, type Escrow, type EscrowEvents } from "@/lib/schemas";
import { hashSecret, loadSecret } from "@/lib/secret";

const POLL_MS = 2000;
const RETRY_MS = 4000;

const asApiError = (err: unknown) =>
  err instanceof ApiError ? err : new ApiError("unknown", describeError(err), 0);

/**
 * Poll the escrow every 2 seconds until it reaches a final state. Errors are kept (so the page can
 * say "reconnecting") and polling continues, except for 404 and 401 which won't fix themselves.
 */
export function useEscrow(id: string) {
  const [escrow, setEscrow] = useState<Escrow | null>(null);
  const [error, setError] = useState<ApiError | null>(null);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      let delay = POLL_MS;
      try {
        const e = await api.get(id);
        if (!alive) return;
        setEscrow(e);
        setError(null);
        if (FINAL_STATES.has(e.state)) return;
      } catch (err) {
        if (!alive) return;
        const ae = asApiError(err);
        setError(ae);
        if (ae.code === "not_found" || ae.code === "unauthorized") return;
        delay = ae.retryAfter ? ae.retryAfter * 1000 : RETRY_MS;
      }
      timer = setTimeout(poll, delay);
    };
    poll();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [id]);

  const refresh = useCallback(async () => {
    try {
      setEscrow(await api.get(id));
      setError(null);
    } catch (err) {
      setError(asApiError(err));
    }
  }, [id]);

  return { escrow, setEscrow, error, refresh };
}

/** The platform's own arbiter (its user id), or null when the server has none. */
export function useDefaultArbiter(): string | null {
  const [id, setId] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    api
      .defaultArbiter()
      .then((d) => alive && setId(d.id))
      .catch(() => {
        // Only a label; without it the escrow just says "an arbiter".
      });
    return () => {
      alive = false;
    };
  }, []);
  return id;
}

/** The audit log and node events, refetched whenever the escrow changes. */
export function useEvents(id: string, version: string | null) {
  const [events, setEvents] = useState<EscrowEvents | null>(null);
  useEffect(() => {
    if (version === null) return;
    let alive = true;
    api
      .events(id)
      .then((ev) => alive && setEvents(ev))
      .catch(() => {
        // The timeline is secondary; the next change retries.
      });
    return () => {
      alive = false;
    };
  }, [id, version]);
  return events;
}

export type StoredSecret =
  | { status: "checking" }
  | { status: "missing" }
  | { status: "mismatch" }
  | { status: "ok"; secret: string };

/**
 * The buyer's secret for this escrow, checked against the escrow's payment hash so we never show a
 * release QR that can't work. `version` lets the caller re-read after saving a new secret.
 */
export function useStoredSecret(escrowId: string, paymentHash: string | null, version: number): StoredSecret {
  const [result, setResult] = useState<{ key: string; value: StoredSecret } | null>(null);
  const key = `${escrowId}|${paymentHash}|${version}`;

  useEffect(() => {
    let alive = true;
    const secret = loadSecret(escrowId);
    const check = async (): Promise<StoredSecret> => {
      if (!secret) return { status: "missing" };
      if (!paymentHash) return { status: "ok", secret };
      return (await hashSecret(secret)) === paymentHash.toLowerCase()
        ? { status: "ok", secret }
        : { status: "mismatch" };
    };
    check().then((value) => alive && setResult({ key, value }));
    return () => {
      alive = false;
    };
  }, [escrowId, paymentHash, key]);

  return result?.key === key ? result.value : { status: "checking" };
}
