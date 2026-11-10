package com.imageannotation.desktop

import android.app.Activity
import android.content.ClipData
import android.content.Intent
import androidx.core.content.FileProvider
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin
import java.io.File

@InvokeArg
class OpenExportArgs {
  lateinit var path: String
  lateinit var fileName: String
}

@TauriPlugin
class HostingFilesPlugin(private val activity: Activity) : Plugin(activity) {
  @Command
  fun openExport(invoke: Invoke) {
    try {
      val args = invoke.parseArgs(OpenExportArgs::class.java)
      val file = File(args.path).canonicalFile
      val exportsRoot = File(activity.filesDir, "exports").canonicalFile
      val rootPath = exportsRoot.path + File.separator
      if (!file.isFile || !file.path.startsWith(rootPath)) {
        throw IllegalArgumentException("export file is outside the protected exports directory")
      }
      if (args.fileName != file.name || !args.fileName.matches(Regex("dataset-export-[0-9a-fA-F-]{36}\\.zip"))) {
        throw IllegalArgumentException("invalid export filename")
      }

      val authority = "${activity.packageName}.fileprovider"
      val contentUri = FileProvider.getUriForFile(activity, authority, file)
      val intent = Intent(Intent.ACTION_SEND).apply {
        type = "application/zip"
        putExtra(Intent.EXTRA_STREAM, contentUri)
        putExtra(Intent.EXTRA_TITLE, args.fileName)
        clipData = ClipData.newRawUri(args.fileName, contentUri)
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
      }
      val chooser = Intent.createChooser(intent, "分享或保存导出文件").apply {
        addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
      }
      activity.startActivity(chooser)
      invoke.resolve()
    } catch (error: Exception) {
      invoke.reject(error.message ?: "无法打开导出文件")
    }
  }
}
