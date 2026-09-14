using System;
using System.Collections.Generic;
using System.IO;
using System.IO.Pipes;
using System.Threading;
using System.Threading.Tasks;

namespace Downloader.Ui.Ipc;

/// <summary>
/// З'єднання з ядром — те саме, яким користується CLI.
/// </summary>
/// <remarks>
/// Вікно не має власної короткої дороги до рушія. Якби мало, різниця між ним
/// і CLI накопичувалась би непомітно, і перша ж функція, зроблена «тільки для
/// вікна», лишилася б неперевіреною.
///
/// <para>
/// Канал локальний і лише локальний: named pipe на Windows, unix-сокет на
/// решті. Жодного TCP-порту, навіть на 127.0.0.1 — саме на локальному
/// HTTP-сервісі pyLoad отримав pre-auth RCE, а gopeed через нього змушує
/// людину вручну вводити порт і токен.
/// </para>
/// </remarks>
public sealed class CoreClient : IAsyncDisposable
{
    private readonly Stream _stream;

    private CoreClient(Stream stream) => _stream = stream;

    /// <summary>
    /// Під'єднатись і привітатись.
    /// </summary>
    /// <exception cref="CoreNotRunningException">
    /// Ядро не запущене. Це звичайна ситуація, а не збій: вікно має
    /// запустити його або сказати про це людині.
    /// </exception>
    public static async Task<CoreClient> ConnectAsync(CancellationToken ct = default)
    {
        Stream stream = await OpenAsync(ct).ConfigureAwait(false);
        var client = new CoreClient(stream);

        await client.HandshakeAsync(ct).ConfigureAwait(false);
        return client;
    }

    private static async Task<Stream> OpenAsync(CancellationToken ct)
    {
        if (OperatingSystem.IsWindows())
        {
            var pipe = new NamedPipeClientStream(
                ".",
                Wire.PipeName,
                PipeDirection.InOut,
                PipeOptions.Asynchronous);

            try
            {
                // Коротке очікування: ядро могло щойно стартувати й ще не
                // встигнути підняти канал. Довше чекати немає сенсу —
                // краще чесно сказати, що його немає.
                await pipe.ConnectAsync(2000, ct).ConfigureAwait(false);
            }
            catch (TimeoutException e)
            {
                pipe.Dispose();
                throw new CoreNotRunningException(e);
            }
            catch (IOException e)
            {
                pipe.Dispose();
                throw new CoreNotRunningException(e);
            }

            return pipe;
        }

        var socket = new System.Net.Sockets.Socket(
            System.Net.Sockets.AddressFamily.Unix,
            System.Net.Sockets.SocketType.Stream,
            System.Net.Sockets.ProtocolType.Unspecified);

        try
        {
            await socket
                .ConnectAsync(new System.Net.Sockets.UnixDomainSocketEndPoint(Wire.PipeName), ct)
                .ConfigureAwait(false);
        }
        catch (System.Net.Sockets.SocketException e)
        {
            socket.Dispose();
            throw new CoreNotRunningException(e);
        }

        return new System.Net.Sockets.NetworkStream(socket, ownsSocket: true);
    }

    /// <summary>
    /// Рукостискання. Має бути першим повідомленням — інакше ядро відмовить.
    /// </summary>
    private async Task HandshakeAsync(CancellationToken ct)
    {
        await Frame
            .WriteAsync(_stream, new HelloRequest("ui", Wire.ProtocolVersion), ct)
            .ConfigureAwait(false);

        Response? resp = await Frame.ReadAsync<Response>(_stream, ct).ConfigureAwait(false)
            ?? throw new IpcException("ядро закрило з'єднання під час рукостискання");

        if (resp.IsError)
        {
            // Найімовірніше — розбіжність версій. Текст від ядра вже
            // пояснює людині, що робити.
            throw new IpcException(resp.Message ?? "ядро відмовило без пояснення");
        }

        if (resp.ProtocolVersion is { } v && v != Wire.ProtocolVersion)
        {
            throw new IpcException(
                $"ядро говорить протоколом {v}, вікно — {Wire.ProtocolVersion}; " +
                "оновіть програму цілком");
        }
    }

    /// <summary>Надіслати запит і дочекатись відповіді.</summary>
    public async Task<Response> CallAsync(Request request, CancellationToken ct = default)
    {
        await Frame.WriteAsync(_stream, request, ct).ConfigureAwait(false);

        return await Frame.ReadAsync<Response>(_stream, ct).ConfigureAwait(false)
            ?? throw new IpcException("ядро закрило з'єднання, не відповівши");
    }

    /// <summary>
    /// Перетворити з'єднання на потік подій.
    /// </summary>
    /// <remarks>
    /// Після цього слати запити цим з'єднанням не можна — воно належить
    /// подіям. Вікно тримає два з'єднання: одне для команд, друге слухає.
    /// </remarks>
    public async IAsyncEnumerable<CoreEvent> SubscribeAsync(
        [System.Runtime.CompilerServices.EnumeratorCancellation] CancellationToken ct = default)
    {
        await Frame.WriteAsync(_stream, new SubscribeRequest(), ct).ConfigureAwait(false);
        _ = await Frame.ReadAsync<Response>(_stream, ct).ConfigureAwait(false);

        while (!ct.IsCancellationRequested)
        {
            CoreEvent? ev;
            try
            {
                ev = await Frame.ReadAsync<CoreEvent>(_stream, ct).ConfigureAwait(false);
            }
            catch (IOException)
            {
                // Ядро зупинилось — для вікна це не помилка, а сигнал
                // перепід'єднатись.
                yield break;
            }

            if (ev is null)
            {
                yield break;
            }

            yield return ev;
        }
    }

    public async ValueTask DisposeAsync()
    {
        await _stream.DisposeAsync().ConfigureAwait(false);
    }
}

/// <summary>Ядро не запущене.</summary>
public sealed class CoreNotRunningException : Exception
{
    public CoreNotRunningException(Exception inner)
        : base(
            "ядро не запущене — запустіть downloader-core (воно живе у треї " +
            "й тримає завантаження)",
            inner)
    {
    }
}
