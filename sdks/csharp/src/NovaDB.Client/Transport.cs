using System;
using System.Collections.Generic;
using System.Net.Http;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

namespace NovaDB.Client
{
    /// <summary>Raw HTTP response handed back by a transport.</summary>
    public sealed class TransportResponse
    {
        public int Status { get; }
        public string Body { get; }

        public TransportResponse(int status, string body)
        {
            Status = status;
            Body = body ?? string.Empty;
        }
    }

    /// <summary>
    /// Pluggable HTTP layer. The default <see cref="HttpClientTransport"/> works on
    /// .NET and on Unity standalone/mobile. Unity WebGL has no sockets, so use a
    /// UnityWebRequest-based transport there (see the Unity example).
    /// Implementations should throw <see cref="TimeoutException"/> on timeouts.
    /// </summary>
    public interface IHttpTransport
    {
        Task<TransportResponse> SendAsync(
            string method,
            string url,
            IReadOnlyDictionary<string, string> headers,
            string jsonBody,
            TimeSpan timeout,
            CancellationToken cancellationToken);
    }

    /// <summary>Default transport based on <see cref="HttpClient"/>.</summary>
    public sealed class HttpClientTransport : IHttpTransport, IDisposable
    {
        private readonly HttpClient _http;
        private readonly bool _ownsClient;

        public HttpClientTransport() : this(new HttpClient { Timeout = Timeout.InfiniteTimeSpan }, true) { }

        public HttpClientTransport(HttpClient client, bool ownsClient = false)
        {
            _http = client ?? throw new ArgumentNullException(nameof(client));
            _ownsClient = ownsClient;
        }

        public async Task<TransportResponse> SendAsync(
            string method,
            string url,
            IReadOnlyDictionary<string, string> headers,
            string jsonBody,
            TimeSpan timeout,
            CancellationToken cancellationToken)
        {
            using (var request = new HttpRequestMessage(new HttpMethod(method), url))
            using (var cts = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken))
            {
                foreach (var header in headers)
                {
                    if (header.Key.Equals("Content-Type", StringComparison.OrdinalIgnoreCase)) continue;
                    request.Headers.TryAddWithoutValidation(header.Key, header.Value);
                }
                if (jsonBody != null) request.Content = new StringContent(jsonBody, Encoding.UTF8, "application/json");
                if (timeout > TimeSpan.Zero) cts.CancelAfter(timeout);
                try
                {
                    using (var response = await _http.SendAsync(request, cts.Token).ConfigureAwait(false))
                    {
                        var body = await response.Content.ReadAsStringAsync().ConfigureAwait(false);
                        return new TransportResponse((int)response.StatusCode, body);
                    }
                }
                catch (OperationCanceledException ex) when (!cancellationToken.IsCancellationRequested)
                {
                    throw new TimeoutException("request timed out after " + timeout.TotalMilliseconds + " ms", ex);
                }
            }
        }

        public void Dispose()
        {
            if (_ownsClient) _http.Dispose();
        }
    }
}
