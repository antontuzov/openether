import type { LlmProvider, LlmRequest, LlmResponse, ToolCall } from '../types.js';

const ANTHROPIC_URL = 'https://api.anthropic.com/v1/messages';

interface AnthropicContentBlock {
  type: string;
  text?: string;
  id?: string;
  name?: string;
  input?: unknown;
}

interface AnthropicResponse {
  content?: AnthropicContentBlock[];
  usage?: { input_tokens?: number; output_tokens?: number };
}

/**
 * Anthropic Messages provider with tool use.
 */
export class AnthropicProvider implements LlmProvider {
  readonly name = 'anthropic';

  private readonly apiKey: string;
  private readonly baseUrl: string;
  private readonly model: string;

  constructor(options: { apiKey: string; model: string; baseUrl?: string }) {
    this.apiKey = options.apiKey;
    this.model = options.model;
    this.baseUrl = (options.baseUrl ?? 'https://api.anthropic.com').replace(/\/$/, '');
  }

  async complete(request: LlmRequest): Promise<LlmResponse> {
    if (!this.apiKey) {
      throw new Error('the Anthropic provider needs an API key');
    }

    const system = request.messages
      .filter((message) => message.role === 'system')
      .map((message) => message.content)
      .join('\n\n');

    const messages = request.messages
      .filter((message) => message.role !== 'system')
      .map((message) => {
        if (message.role === 'tool') {
          return {
            role: 'user',
            content: [
              {
                type: 'tool_result',
                tool_use_id: message.toolCallId ?? 'call',
                content: message.content,
              },
            ],
          };
        }
        return { role: message.role, content: message.content };
      });

    const body: Record<string, unknown> = {
      model: this.model,
      max_tokens: request.maxTokens ?? 1200,
      temperature: request.temperature ?? 0.2,
      system,
      messages,
    };
    if (request.tools.length > 0) {
      body.tools = request.tools.map((tool) => ({
        name: tool.name,
        description: tool.description,
        input_schema: tool.inputSchema,
      }));
    }

    const response = await fetch(`${this.baseUrl}/v1/messages`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        'x-api-key': this.apiKey,
        'anthropic-version': '2023-06-01',
      },
      body: JSON.stringify(body),
    });

    if (!response.ok) {
      const text = await response.text();
      throw new Error(`LLM request failed with ${response.status}: ${text.slice(0, 500)}`);
    }

    const payload = (await response.json()) as AnthropicResponse;
    const blocks = payload.content ?? [];
    const content = blocks
      .filter((block) => block.type === 'text')
      .map((block) => block.text ?? '')
      .join('\n')
      .trim();

    const toolCalls: ToolCall[] = blocks
      .filter((block) => block.type === 'tool_use')
      .map((block, index) => ({
        id: block.id ?? `call_${index}`,
        name: block.name ?? '',
        arguments:
          block.input && typeof block.input === 'object'
            ? (block.input as Record<string, unknown>)
            : {},
      }));

    return {
      content,
      toolCalls,
      usage: {
        promptTokens: payload.usage?.input_tokens,
        completionTokens: payload.usage?.output_tokens,
      },
    };
  }
}
