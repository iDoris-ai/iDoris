export {
  QUANT_TABLE,
  KV_BYTES_PER_ELEMENT,
  DEFAULT_OVERHEAD_GB,
  WIRED_PCT,
  bytesToGb,
  bytesToMib,
  bytesToGib,
  weightBytes,
  weightsGb,
  kvBytes,
  kvCacheGb,
  footprintGb,
  appleReserveGb,
  appleUsableGb,
  recommendedWiredLimitMb,
  lookupQuant,
} from "./memory.js";
export type { WiredMode, KvQuant, ModelArch, QuantSpec, QuantEntry, FootprintInput } from "./memory.js";

export {
  UNKNOWN_CHIP,
  nominalRamGb,
  parseChip,
  parseSystemProfilerGpuCores,
  makeHostFacts,
  inspectHost,
  withSystemProfiler,
} from "./probe.js";
export type { HostFacts, HostFactsInput } from "./probe.js";

export {
  CatalogError,
  parseCatalog,
  loadCatalog,
  recommend,
  recommendFromFile,
  DEFAULT_POLICY,
  TEMP_SLOT_RESERVE_GB,
  HEADROOM_GB,
} from "./recommend.js";
export type {
  Capability,
  AdmissionStatus,
  CatalogQuant,
  CatalogModel,
  CatalogExcluded,
  Catalog,
  RecommenderPolicy,
  QuantPick,
  ResidentChoice,
  TempChoice,
  BlockedChoice,
  Recommendation,
  RecommendInput,
} from "./recommend.js";
