import type {
  CompanyStatus,
  IssueThreadInteractionKind,
  IssueThreadInteractionResolverPolicy,
  PauseReason,
} from "../constants.js";

export interface InteractionResolverKindGovernance {
  defaultPolicy?: IssueThreadInteractionResolverPolicy;
  cap?: IssueThreadInteractionResolverPolicy;
}

export type InteractionResolverGovernance = Partial<
  Record<IssueThreadInteractionKind, InteractionResolverKindGovernance>
>;

/**
 * Persisted per-company run-scheduling gate configuration
 * (`companies.scheduling`). Stored shape is partial: absent keys fall back to
 * the host defaults at read time.
 */
export type CompanySchedulingConfig = Partial<{
  /** Gate the company-wide in-flight run cap. False restores pre-gate behavior. */
  enabled: boolean;
  /** Maximum runs in flight (started + reserved) company-wide at claim time. */
  maxInFlight: number;
}>;

/** Defaults applied to absent CompanySchedulingConfig keys. */
export const COMPANY_SCHEDULING_DEFAULTS = {
  enabled: true,
  maxInFlight: 8,
} as const;

/** Upper bound for CompanySchedulingConfig.maxInFlight accepted via the API. */
export const COMPANY_SCHEDULING_MAX_INFLIGHT_CAP = 256;

export interface Company {
  id: string;
  name: string;
  description: string | null;
  status: CompanyStatus;
  pauseReason: PauseReason | null;
  pausedAt: Date | null;
  issuePrefix: string;
  issueCounter: number;
  budgetMonthlyCents: number;
  spentMonthlyCents: number;
  defaultResponsibleUserId: string | null;
  requireBoardApprovalForNewAgents: boolean;
  interactionResolverGovernance: InteractionResolverGovernance;
  feedbackDataSharingEnabled: boolean;
  feedbackDataSharingConsentAt: Date | null;
  feedbackDataSharingConsentByUserId: string | null;
  feedbackDataSharingTermsVersion: string | null;
  logoAssetId: string | null;
  logoUrl: string | null;
  createdAt: Date;
  updatedAt: Date;
}
