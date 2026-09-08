package com.luminavideo.bridge

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ImageOutputPolicyTest {
    @Test
    fun opaqueAndVendorAllocationsNeverReachCpuPlaneAccess() {
        assertFalse(isCpuReadableYuvImage(34, 34, 3))
        // ImageReader can report YUV while the codec retained an opaque allocation.
        assertFalse(isCpuReadableYuvImage(35, 0x7fa30c06, 3))
        assertFalse(isCpuReadableYuvImage(35, 35, 0x900))
    }

    @Test
    fun readableYuvAllowsBothCpuReadUsageModes() {
        assertTrue(isCpuReadableYuvImage(35, 35, 2))
        assertTrue(isCpuReadableYuvImage(35, 35, 3))
    }
}
