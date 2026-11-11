import type { AgentConfig } from '../config.js';
import type { LlmProvider } from '../types.js';
import { AnthropicProvider } from './anthropic.js';
import { LocalPlannerProvider } from './local.js';
import { OpenAiProvider } from './openai.js';

export { AnthropicProvider } from './anthropic.js';
export { LocalPlannerProvider, type CycleContext } from './local.js';
export { OpenAiProvider, parseArguments } from './openai.js';

/**
 * Builds the model client from configuration.
 *
 * The offline planner is the default so the agent is runnable without an API
 * key; a hosted provider is only selected when its key is present.
 */
export function createProvider(config: AgentConfig): LlmProvider {
  switch (config.llmProvider) {
    case 'openai':
      return new OpenAiProvider({
        apiKey: config.openaiApiKey,
        model: config.llmModel,
        baseUrl: config.llmBaseUrl || undefined,
      });
    case 'openai-compatible':
      return new OpenAiProvider({
        apiKey: config.openaiApiKey,
        model: config.llmModel,
        baseUrl: config.llmBaseUrl || undefined,
        name: 'openai-compatible',
      });
    case 'anthropic':
      return new AnthropicProvider({
        apiKey: config.anthropicApiKey,
        model: config.llmModel,
        baseUrl: config.llmBaseUrl || undefined,
      });
    case 'local':
      return new LocalPlannerProvider();
    default: {
      const exhaustive: never = config.llmProvider;
      throw new Error(`unsupported LLM provider: ${String(exhaustive)}`);
    }
  }
}
