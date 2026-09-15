using System;
using System.Buffers;
using System.Buffers.Binary;
using System.IO;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace Downloader.Ui.Ipc;

/// <summary>
/// Кадрування повідомлень: чотири байти little-endian довжини, далі JSON у UTF-8.
/// </summary>
/// <remarks>
/// Дзеркало <c>crates/ipc/src/frame.rs</c>. Два незалежні втілення одного
/// протоколу — місце, де розбіжність виявляється не одразу, тому кожне
/// рішення тут повторює рушій буквально, а не «по суті».
///
/// <para>
/// Чому не «рядок до \n»: ім'я файла приходить із мережі й може містити
/// перенос рядка. Довжина попереду знімає це питання цілком.
/// </para>
/// </remarks>
public static class Frame
{
    /// <summary>Найбільший припустимий кадр — 16 МіБ, як і в ядрі.</summary>
    public const int MaxFrame = 16 * 1024 * 1024;

    private static readonly JsonSerializerOptions Json = new()
    {
        // Ядро шле теґовані перелічення у snake_case: {"kind":"add",...}.
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        PropertyNameCaseInsensitive = true,
        DefaultIgnoreCondition = System.Text.Json.Serialization.JsonIgnoreCondition.WhenWritingNull,
    };

    /// <summary>Налаштування серіалізації, спільні для всього протоколу.</summary>
    public static JsonSerializerOptions Options => Json;

    /// <summary>Надіслати повідомлення.</summary>
    public static async Task WriteAsync<T>(Stream stream, T value, CancellationToken ct = default)
    {
        byte[] body = JsonSerializer.SerializeToUtf8Bytes(value, Json);

        if (body.Length > MaxFrame)
        {
            throw new IpcException(
                $"кадр завеликий: {body.Length} байтів при межі {MaxFrame}");
        }

        // Об'єднуємо заголовок і тіло в один буфер для єдиного системного виклику у named pipe.
        byte[] packet = new byte[4 + body.Length];
        BinaryPrimitives.WriteUInt32LittleEndian(packet.AsSpan(0, 4), (uint)body.Length);
        body.CopyTo(packet.AsSpan(4));

        await stream.WriteAsync(packet, ct).ConfigureAwait(false);
        await stream.FlushAsync(ct).ConfigureAwait(false);
    }

    /// <summary>
    /// Прочитати повідомлення. <c>null</c> означає, що співрозмовник пішов —
    /// це звичайний кінець розмови, а не збій.
    /// </summary>
    public static async Task<T?> ReadAsync<T>(Stream stream, CancellationToken ct = default)
        where T : class
    {
        byte[] header = new byte[4];
        if (!await ReadExactAsync(stream, header, ct).ConfigureAwait(false))
        {
            return null;
        }

        uint size = BinaryPrimitives.ReadUInt32LittleEndian(header);

        // ⚠️ Перевірка **до** виділення пам'яті. Інакше чотири байти
        // FF FF FF FF змусили б процес просити чотири гігабайти — локальна
        // відмова в обслуговуванні коштом одного пакета.
        if (size > MaxFrame)
        {
            throw new IpcException($"кадр завеликий: {size} байтів при межі {MaxFrame}");
        }

        // Використовуємо пул пам'яті, щоб не навантажувати GC при частому читанні знімків.
        byte[] rented = ArrayPool<byte>.Shared.Rent((int)size);
        try
        {
            Memory<byte> memory = rented.AsMemory(0, (int)size);
            if (!await ReadExactAsync(stream, memory, ct).ConfigureAwait(false))
            {
                // Обрізане тіло — те саме, що обірване з'єднання.
                return null;
            }

            return JsonSerializer.Deserialize<T>(memory.Span, Json);
        }
        catch (JsonException e)
        {
            throw new IpcException($"кадр не є коректним JSON: {e.Message}", e);
        }
        finally
        {
            ArrayPool<byte>.Shared.Return(rented);
        }
    }

    /// <summary>Прочитати рівно стільки байтів, скільки просили.</summary>
    /// <returns><c>false</c>, якщо потік закінчився раніше.</returns>
    private static async Task<bool> ReadExactAsync(
        Stream stream,
        Memory<byte> buffer,
        CancellationToken ct)
    {
        int filled = 0;
        while (filled < buffer.Length)
        {
            int read = await stream.ReadAsync(buffer[filled..], ct).ConfigureAwait(false);
            if (read == 0)
            {
                return false;
            }
            filled += read;
        }
        return true;
    }
}

/// <summary>Помилка обміну з ядром.</summary>
public sealed class IpcException : Exception
{
    public IpcException(string message) : base(message) { }
    public IpcException(string message, Exception inner) : base(message, inner) { }
}
