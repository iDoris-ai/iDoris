export { HealthTracker, FAILURE_THRESHOLD, COOLDOWN_MS } from "./health.js";
export { parseProfile, currentDeployMode, ProfileError } from "./profile.js";
export type { HeaderBag, ProfileParseResult } from "./profile.js";
export {
  UtteranceIntentDetector,
  cosine,
  defaultIntentDetector,
  hashingEmbedder,
  queryText,
  resolveProfile,
  DEFAULT_INTENT_ROUTES,
} from "./intent.js";
export type {
  Embedder,
  IntentContext,
  IntentDetector,
  IntentHit,
  IntentSource,
  ResolvedProfile,
  UtteranceDetectorOptions,
  UtteranceRoute,
} from "./intent.js";
export { decide, loadRoutingPolicy } from "./policy.js";
export type { RouteDecision } from "./policy.js";
export { loadComponents, ComponentRegistrationError } from "./registry.js";
export type { LoadComponentsOptions, Registered } from "./registry.js";
export { startRouter } from "./server.js";
export type { Router, RouterOptions } from "./server.js";
export { dispatch, isLocalCapable } from "./dispatch.js";
export type { DispatchOutcome, EgressCounter } from "./dispatch.js";
export {
  EgressGuardError,
  assertSubscriptionSource,
  isAllowedSubscriptionSource,
  isLoopbackAddress,
  isNonPersonalDeployMode,
  isTailscaleAddress,
  subscriptionStartupGate,
} from "./egress-guard.js";
export type {
  EgressGuardErrorCode,
  SubscriptionStartupAction,
  SubscriptionStartupDecision,
} from "./egress-guard.js";
export { decisionReason, requireReason, reasonHeader, MissingReasonError, REASON_KINDS } from "./reason.js";
export type { DecisionReason, ReasonKind } from "./reason.js";
export { ChatProxy } from "./proxy.js";
export type { FetchLike, FetchResponseLike, ForwardResult, ReadableStreamLike } from "./proxy.js";
export { EvictionLock, OomError } from "./evict-lock.js";
export { DefaultCapabilitiesProvider, defaultCatalogPath } from "./capabilities.js";
export type {
  AdmissionStatus,
  CapabilityEntry,
  CapabilitiesProvider,
  CapabilitiesOptions,
} from "./capabilities.js";
export {
  AuditWriter,
  AUDIT_FIELDS,
  CONTENT_FIELD_BLACKLIST,
  MAX_FIELD_CHARS,
  validateAuditReason,
  AuditError,
  ContentLeakError,
  UnknownAuditFieldError,
  FieldTooLongError,
  NonScalarAuditValueError,
  InvalidReasonError,
} from "./audit.js";
export type { AuditField, AuditInput, AuditRecord, AuditScalar } from "./audit.js";
