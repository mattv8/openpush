package dev.openpush.mobile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.assertThrows
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.io.File
import java.nio.file.Files

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34])
class GatewayHttpMediaTest {
    @Test fun streamsFixedLengthUploadAndBoundedDownload() {
        val uploaded = mutableListOf<LoopbackHttpServer.Request>()
        LoopbackHttpServer { request ->
            uploaded += request
            if (request.method == "PUT") LoopbackHttpServer.Response(204) else LoopbackHttpServer.Response(200, "cipher")
        }.use { server ->
            val source = Files.createTempFile("cipher", ".bin").toFile().apply { writeText("cipher") }
            val destination = Files.createTempFile("download", ".part").toFile().apply { delete() }
            val http = GatewayHttp(server.origin, "token")
            assertTrue(http.putFile("/v1/attachments/abc/upload", source, 6).ok)
            assertEquals("Bearer token", uploaded.single().headers["authorization"])
            assertEquals("cipher", uploaded.single().body)
            assertTrue(http.getToFile("/v1/attachments/abc", destination, 6).ok)
            assertEquals("cipher", destination.readText())
            source.delete(); destination.delete()
        }
    }

    @Test fun jsonGetPreservesLegitimateQueryParameters() {
        var target: String? = null
        LoopbackHttpServer { request ->
            target = request.target
            LoopbackHttpServer.Response(200, "[]")
        }.use { server ->
            val response = GatewayHttp(server.origin, "token").get("/v1/events?after=7&limit=50")
            assertTrue(response.ok)
            assertEquals("/v1/events?after=7&limit=50", target)
        }
    }

    @Test fun rejectsNonV1Paths() {
        val file = Files.createTempFile("cipher", ".bin").toFile()
        try { assertThrows(IllegalArgumentException::class.java) { GatewayHttp("http://127.0.0.1", "token").putFile("/other", file, 0) } }
        finally { file.delete() }
    }

    @Test fun binaryPathsRejectQueriesAndFailedGetsDeleteStaleDestination() {
        val destination = Files.createTempFile("download", ".part").toFile().apply { writeText("stale") }
        LoopbackHttpServer { LoopbackHttpServer.Response(404, "{\"code\":\"attachment_unavailable\"}") }.use { server ->
            val http = GatewayHttp(server.origin, "token")
            assertThrows(IllegalArgumentException::class.java) {
                http.getToFile("/v1/attachments/abc?variant=1", destination, 8)
            }
            assertTrue(destination.exists())
            val result = http.getToFile("/v1/attachments/abc", destination, 8)
            assertEquals(404, result.code)
            assertFalse(destination.exists())
        }
    }

    @Test fun rejectsMismatchedUploadBeforeOpeningConnection() {
        val source = Files.createTempFile("cipher", ".bin").toFile().apply { writeText("abc") }
        var requests = 0
        LoopbackHttpServer { requests++; LoopbackHttpServer.Response(204) }.use { server ->
            assertThrows(GatewayTransportException::class.java) { GatewayHttp(server.origin, "token").putFile("/v1/a", source, 4) }
            assertEquals(0, requests)
        }
        source.delete()
    }

    @Test fun deletesDestinationForBoundedGetAndParsesBoundedError() {
        LoopbackHttpServer { request ->
            when (request.target) {
                "/v1/large" -> LoopbackHttpServer.Response(200, "x".repeat(9))
                else -> LoopbackHttpServer.Response(413, """{"code":"attachment_too_large","detail":"${"x".repeat(1_000)}"}""")
            }
        }.use { server ->
            val destination = Files.createTempFile("download", ".part").toFile()
            assertThrows(GatewayTransportException::class.java) { GatewayHttp(server.origin, "token").getToFile("/v1/large", destination, 8) }
            assertFalse(destination.exists())
            val error = GatewayHttp(server.origin, "token").get("/v1/error")
            assertEquals("attachment_too_large", error.errorCode)
            assertTrue(error.body!!.length <= GatewayHttp.MAX_ERROR_BYTES)
        }
    }
}
