export const FAILURE_THRESHOLD = 3;
export const COOLDOWN_MS = 30_000;

export interface HealthState {
  consecutiveFailures: number;
  cooldownUntil: number;
}

export class HealthTracker {
  private readonly state = new Map<string, HealthState>();

  record(providerId: string, ok: boolean, now: number = Date.now()): void {
    const s = this.state.get(providerId) ?? { consecutiveFailures: 0, cooldownUntil: 0 };
    if (ok) {
      s.consecutiveFailures = 0;
      s.cooldownUntil = 0;
    } else {
      s.consecutiveFailures += 1;
      if (s.consecutiveFailures >= FAILURE_THRESHOLD) s.cooldownUntil = now + COOLDOWN_MS;
    }
    this.state.set(providerId, s);
  }

  isCoolingDown(providerId: string, now: number = Date.now()): boolean {
    return (this.state.get(providerId)?.cooldownUntil ?? 0) > now;
  }

  snapshot(): Record<string, HealthState> {
    return Object.fromEntries(this.state);
  }
}
