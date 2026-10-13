/**
 * Node.js `fetch` replacement that reuses TCP connections through an
 * `http.Agent({ keepAlive: true })` pool. Node's built-in `fetch` (undici)
 * already keeps connections alive for a few seconds; use this when you want an
 * explicit, larger pool (`maxSockets`) and a long idle timeout for very high
 * request rates, e.g. a game server talking to NovaDB. Node only.
 */
export interface KeepAliveFetchOptions {
  /** Most sockets per host. Default 256. */
  maxSockets?: number;
  /** Sockets kept open while idle. Default 64. */
  maxFreeSockets?: number;
  /** TCP keep-alive probe delay in ms. Default 30000. */
  keepAliveMsecs?: number;
}

type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;
/* eslint-disable @typescript-eslint/no-explicit-any */

export function createKeepAliveFetch(options: KeepAliveFetchOptions = {}): FetchLike {
  let loaded: Promise<{ http: any; https: any; httpAgent: any; httpsAgent: any }> | undefined;
  const load = (): Promise<{ http: any; https: any; httpAgent: any; httpsAgent: any }> =>
    (loaded ??= (async () => {
      const httpName: string = "node:http";
      const httpsName: string = "node:https";
      const http: any = await import(httpName);
      const https: any = await import(httpsName);
      const agentOptions = {
        keepAlive: true,
        keepAliveMsecs: options.keepAliveMsecs ?? 30_000,
        maxSockets: options.maxSockets ?? 256,
        maxFreeSockets: options.maxFreeSockets ?? 64,
      };
      return { http, https, httpAgent: new http.Agent(agentOptions), httpsAgent: new https.Agent(agentOptions) };
    })());

  return async (input, init = {}) => {
    const { http, https, httpAgent, httpsAgent } = await load();
    const url = new URL(input);
    const secure = url.protocol === "https:";
    const headers: Record<string, string> = {};
    new Headers(init.headers as HeadersInit | undefined).forEach((value, key) => {
      headers[key] = value;
    });
    const body = init.body === undefined || init.body === null ? undefined : String(init.body);
    if (body !== undefined) headers["content-length"] = String(new TextEncoder().encode(body).length);
    return new Promise<Response>((resolve, reject) => {
      const request = (secure ? https : http).request(
        url,
        { method: init.method ?? "GET", headers, agent: secure ? httpsAgent : httpAgent },
        (res: any) => {
          const chunks: Uint8Array[] = [];
          res.on("data", (chunk: Uint8Array) => chunks.push(chunk));
          res.on("error", reject);
          res.on("end", () => {
            const responseHeaders = new Headers();
            for (const [key, value] of Object.entries(res.headers as Record<string, string | string[]>)) {
              if (Array.isArray(value)) value.forEach((v) => responseHeaders.append(key, v));
              else if (value !== undefined) responseHeaders.set(key, value);
            }
            const status: number = res.statusCode ?? 0;
            const noBody = status === 204 || status === 304;
            resolve(
              new Response(noBody ? null : (globalThis as any).Buffer.concat(chunks) as any, {
                status,
                statusText: res.statusMessage,
                headers: responseHeaders,
              }),
            );
          });
        },
      );
      request.on("error", reject);
      const signal = init.signal;
      if (signal) {
        const abort = (): void => request.destroy(new Error("aborted"));
        if (signal.aborted) abort();
        else signal.addEventListener("abort", abort, { once: true });
      }
      request.end(body);
    });
  };
}
