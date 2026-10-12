// Optional NovaDB transport for Unity, built on UnityWebRequest.
//
// Use it on Unity WebGL (where HttpClient/sockets are unavailable) or whenever
// you prefer Unity's networking stack. Copy this file into your Unity project
// next to the NovaDB.Client sources/DLL. It must be used from the main thread
// (UnityWebRequest requirement), which is the default for MonoBehaviour code.
//
//   var db = new NovaDBClient(new NovaDBOptions {
//       Url = "https://db.example.com", Token = "...", Database = "game",
//       Transport = new UnityWebRequestTransport(),
//   });

using System;
using System.Collections.Generic;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using NovaDB.Client;
using UnityEngine.Networking;

public sealed class UnityWebRequestTransport : IHttpTransport
{
    public Task<TransportResponse> SendAsync(
        string method,
        string url,
        IReadOnlyDictionary<string, string> headers,
        string jsonBody,
        TimeSpan timeout,
        CancellationToken cancellationToken)
    {
        var tcs = new TaskCompletionSource<TransportResponse>();
        var request = new UnityWebRequest(url, method)
        {
            downloadHandler = new DownloadHandlerBuffer(),
        };
        if (jsonBody != null)
        {
            request.uploadHandler = new UploadHandlerRaw(Encoding.UTF8.GetBytes(jsonBody));
        }
        foreach (var header in headers)
        {
            request.SetRequestHeader(header.Key, header.Value);
        }
        if (timeout > TimeSpan.Zero)
        {
            // UnityWebRequest.timeout is in whole seconds (0 = none).
            request.timeout = Math.Max(1, (int)Math.Ceiling(timeout.TotalSeconds));
        }

        var registration = cancellationToken.Register(() =>
        {
            request.Abort();
            tcs.TrySetCanceled(cancellationToken);
        });

        var operation = request.SendWebRequest();
        operation.completed += _ =>
        {
            registration.Dispose();
            try
            {
                if (request.result == UnityWebRequest.Result.ConnectionError)
                {
                    if (request.error != null && request.error.IndexOf("timeout", StringComparison.OrdinalIgnoreCase) >= 0)
                        tcs.TrySetException(new TimeoutException(request.error));
                    else
                        tcs.TrySetException(new Exception(request.error ?? "connection error"));
                    return;
                }
                // Protocol errors (4xx/5xx) still carry a JSON body that the client maps to typed exceptions.
                tcs.TrySetResult(new TransportResponse((int)request.responseCode, request.downloadHandler.text));
            }
            finally
            {
                request.Dispose();
            }
        };
        return tcs.Task;
    }
}
