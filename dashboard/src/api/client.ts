import { t as translate } from '../i18n';
import type { ApiError, ApiResult, Method, Params, Returns } from './types';

/** 一次请求的底层通道:真实环境是 fetch,开发与截图时是内存里的 mock。 */
export type Transport = (
  method: string,
  body: unknown,
  headers: Record<string, string>,
) => Promise<ApiResult<unknown>>;

export class ApiCallError extends Error {
  readonly code: ApiError['code'];
  readonly key?: string;
  constructor(error: ApiError) {
    super(error.message);
    this.code = error.code;
    if (error.key !== undefined) this.key = error.key;
  }
}

export const CSRF_HEADER = 'X-Summon-Csrf';

export const fetchTransport: Transport = async (method, body, headers) => {
  let res: Response;
  try {
    res = await fetch(`/api/${method}`, {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      headers: { 'Content-Type': 'application/json', ...headers },
      body: JSON.stringify(body),
    });
  } catch {
    return { ok: false, error: { code: 'unavailable', message: translate('dbf770f') } };
  }
  const text = await res.text();
  try {
    return JSON.parse(text) as ApiResult<unknown>;
  } catch {
    // 鉴权层在进入业务前就拒绝的请求可能不是 JSON;按状态码给出能看懂的原因。
    if (res.status === 401 || res.status === 403) {
      return { ok: false, error: { code: 'unauthenticated', message: translate('d075903') } };
    }
    return { ok: false, error: { code: 'internal', message: translate('d7a7072', [res.status]) } };
  }
};

export interface Client {
  call<M extends Method>(method: M, params: Params<M>): Promise<Returns<M>>;
  setCsrf(token: string): void;
}

/**
 * 只发 POST JSON。csrf 来自 session.whoami;拿到之前的那一次 whoami 不带这个头。
 * unauthenticated 在这里统一转给外壳,页面各自不用再判。
 */
export function createClient(transport: Transport, onUnauthenticated: () => void): Client {
  let csrf = '';
  return {
    setCsrf(token) {
      csrf = token;
    },
    async call(method, params) {
      const headers: Record<string, string> = csrf ? { [CSRF_HEADER]: csrf } : {};
      const result = await transport(method, params, headers);
      if (result.ok) return result.data as Returns<typeof method>;
      if (result.error.code === 'unauthenticated') onUnauthenticated();
      throw new ApiCallError(result.error);
    },
  };
}

export function errorMessage(err: unknown): string {
  if (err instanceof ApiCallError) return err.message;
  if (err instanceof Error) return err.message;
  return String(err);
}
