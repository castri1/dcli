export interface HookConfig {
  command: string;
  description?: string;
}

export interface PipelineHooks {
  "pre-deploy"?: HookConfig[];
  "post-deploy"?: HookConfig[];
}

export interface PipelineConfig {
  name: string;
  description?: string;
  hooks?: PipelineHooks;
}

export interface EnvironmentConfig {
  profile: string;
  region: string;
  pipelines: PipelineConfig[];
}

export interface DefaultsConfig {
  pollIntervalSeconds: number;
  notifications: boolean;
}

export interface AppConfig {
  environments: Record<string, EnvironmentConfig>;
  defaults: DefaultsConfig;
}

export const DEFAULT_CONFIG: DefaultsConfig = {
  pollIntervalSeconds: 10,
  notifications: true,
};
