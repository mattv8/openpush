package dev.openpush.mobile

import java.io.BufferedInputStream
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import kotlin.concurrent.thread

/** Minimal HTTP/1.1 loopback server for transport tests (android.jar has no JDK httpserver). */
class LoopbackHttpServer(private val handler: (Request) -> Response) : AutoCloseable {
    data class Request(val method: String, val target: String, val headers: Map<String, String>, val body: String, val bodyBytes: ByteArray)
    data class Response(val code: Int, val body: String = "", val headers: Map<String, String> = emptyMap())

    private val socket = ServerSocket(0, 16, InetAddress.getByName("127.0.0.1"))
    val origin = "http://127.0.0.1:${socket.localPort}"
    @Volatile private var closing = false

    private val acceptor = thread(isDaemon = true) {
        while (!closing && !socket.isClosed) {
            val client = try { socket.accept() } catch (_: Exception) { break }
            if (closing) { client.close(); break }
            client.use(::serve)
        }
    }

    private fun serve(client: Socket) {
        val input = BufferedInputStream(client.getInputStream())
        val lines = generateSequence { readLine(input) }.takeWhile { it.isNotEmpty() }.toList()
        val (method, target) = lines.first().split(' ').let { it[0] to it[1] }
        val headers = lines.drop(1).associate { line -> line.substringBefore(':').trim().lowercase() to line.substringAfter(':').trim() }
        val length = headers["content-length"]?.toInt() ?: 0
        val body = ByteArray(length).also { var read = 0; while (read < length) read += input.read(it, read, length - read) }
        val response = handler(Request(method, target, headers, body.toString(Charsets.UTF_8), body))
        val bytes = response.body.toByteArray()
        client.getOutputStream().apply {
            write("HTTP/1.1 ${response.code} X\r\nContent-Length: ${bytes.size}\r\nContent-Type: application/json\r\n".toByteArray())
            response.headers.forEach { (name, value) -> write("$name: $value\r\n".toByteArray()) }
            write("Connection: close\r\n\r\n".toByteArray())
            write(bytes)
            flush()
        }
    }

    private fun readLine(input: InputStream): String? {
        val out = ByteArrayOutputStream()
        while (true) {
            val byte = input.read()
            if (byte < 0) return if (out.size() == 0) null else out.toString()
            if (byte == '\n'.code) return out.toString().trimEnd('\r')
            out.write(byte)
        }
    }

    override fun close() {
        if (closing) return
        closing = true
        // Wake accept normally before closing its descriptor. Cross-thread NIO pre-close
        // uses NativeThread.signal, which fails under the Android builder's x86 emulation.
        Socket().use { it.connect(InetSocketAddress("127.0.0.1", socket.localPort), 1_000) }
        acceptor.join(2_000)
        socket.close()
        check(!acceptor.isAlive) { "Loopback server did not stop" }
    }
}
