import type { LlmProvider, LlmRequest, LlmResponse, ToolCall } from '../types.js';

interface OpenAiToolCall {
  id?: string;
  type?: string;
  function?: { name?: string; arguments?: string };
}

interface OpenAiChoice {
  message?: {
    content?: string | null;
    tool_calls?: OpenAiToolCall[];
  };
  finish_reason?: string;
}

interface OpenAiResponse {
  choices?: OpenAiChoice[];
  usage?: { prompt_tokens?: number; completion_tokens?: number };
}

/**
 * OpenAI chat-completions provider with function calling.
 *
 * The same implementation backs any OpenAI-compatible endpoint by pointing
 * `baseUrl` at it.
 */
export class OpenAiProvider implements LlmProvider {
  readonly name: string;

  private readonly apiKey: string;
  private readonly baseUrl: string;
  private readonly model: string;

  constructor(options: {
    apiKey: string;
    model: string;
    baseUrl?: string;
    name?: string;
  }) {
    this.apiKey = options.apiKey;
    this.model = options.model;
    this.baseUrl = (options.baseUrl ?? 'https://api.openai.com/v1').replace(/\/$/, '');
    this.name = options.name ?? 'openai';
  }

  async complete(request: LlmRequest): Promise<LlmResponse> {
    if (!this.apiKey) {
      throw new Error('the OpenAI provider needs an API key');
    }

    const messages = request.messages.map((message) => {
      if (message.role === 'tool') {
        return {
          role: 'tool',
          tool_call_id: message.toolCallId ?? 'call',
          content: message.content,
        };
      }
      if (message.role === 'assistant') {
        return { role: 'assistant', content: message.content };
      }
      return { role: message.role, content: message.content };
    });

    const body: Record<string, unknown> = {
      model: this.model,
      messages,
      temperature: request.temperature ?? 0.2,
    };
    if (request.maxTokens) {
      body.max_tokens = request.maxTokens;
    }
    if (request.tools.length > 0) {
      body.tools = request.tools.map((tool) => ({
        type: 'function',
        function: {
          name: tool.name,
          description: tool.description,
          parameters: tool.inputSchema,
        },
      }));
      body.tool_choice = 'auto';
    }

    const response = await fetch(`${this.baseUrl}/chat/completions`, {
      method: 'POST',
      headers: {
        'content-type': 'application/json',
        authorization: `Bearer ${this.apiKey}`,
      },
      body: JSON.stringify(body),
    });

    if (!response.ok) {
      const text = await response.text();
      throw new Error(`LLM request failed with ${response.status}: ${text.slice(0, 500)}`);
    }

    const payload = (await response.json()) as OpenAiResponse;
    const choice = payload.choices?.[0];
    const toolCalls: ToolCall[] = (choice?.message?.tool_calls ?? [])
      .filter((call) => call.function?.name)
      .map((call, index) => ({
        id: call.id ?? `call_${index}`,
        name: call.function?.name ?? '',
        arguments: parseArguments(call.function?.arguments),
      }));

    return {
      content: choice?.message?.content ?? '',
      toolCalls,
      usage: {
        promptTokens: payload.usage?.prompt_tokens,
        completionTokens: payload.usage?.completion_tokens,
      },
    };
  }
}

export function parseArguments(raw: string | undefined): Record<string, unknown> {
  if (!raw) {
    return {};
  }
  try {
    const parsed: unknown = JSON.parse(raw);
    return parsed && typeof parsed === 'object' ? (parsed as Record<string, unknown>) : {};
  } catch {
    return {};
  }
}
