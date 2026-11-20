# JNA and the UniFFI bindings are bound by reflection (Native.register, Structure
# fields, callback interfaces): keep them for apps that enable R8.
-keep class com.sun.jna.** { *; }
-keep class * implements com.sun.jna.** { *; }
-keep class uniffi.vault_ffi.** { *; }
-dontwarn java.awt.**
