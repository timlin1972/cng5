package com.moxa.cng5.tablet

import org.junit.Assert.assertEquals
import org.junit.Test

class UrlNormalizerTest {
    @Test
    fun addsHttpPrefixWhenMissing() {
        assertEquals(
            "http://100.100.1.61:9759/tablet",
            UrlNormalizer.normalize("100.100.1.61:9759/tablet")
        )
    }

    @Test
    fun leavesHttpUrlUnchanged() {
        assertEquals("http://foo/tablet", UrlNormalizer.normalize("http://foo/tablet"))
    }

    @Test
    fun leavesHttpsUrlUnchanged() {
        assertEquals("https://foo/tablet", UrlNormalizer.normalize("https://foo/tablet"))
    }

    @Test
    fun trimsWhitespace() {
        assertEquals("http://foo", UrlNormalizer.normalize("  foo  "))
    }
}
