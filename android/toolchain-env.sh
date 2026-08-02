#!/usr/bin/env bash
# Portable Android 建置工具鏈（JDK 17 + SDK cmdline-tools），裝在
# ~/.android-toolchain，不動系統既有的 Java 8，也不需要 sudo。
export JAVA_HOME
JAVA_HOME=$(ls -d /home/moxa/.android-toolchain/jdk-17*/ | head -1)
export ANDROID_HOME=/home/moxa/.android-toolchain/android-sdk
export PATH="$JAVA_HOME/bin:$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH"
