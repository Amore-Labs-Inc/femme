/// What can be done with a picture or video's bytes outside the app: share it, save it to
/// Photos, copy it to the clipboard. Each says plainly when the platform can't.
library;

import 'dart:io';

import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';
import 'package:gal/gal.dart';
import 'package:pasteboard/pasteboard.dart';
import 'package:share_plus/share_plus.dart';

String _ext(String name) {
  final dot = name.lastIndexOf('.');
  if (dot < 0 || dot < name.lastIndexOf('/')) return '';
  return name.substring(dot).toLowerCase();
}

String mimeOf(String name) => switch (_ext(name)) {
  '.png' => 'image/png',
  '.jpg' || '.jpeg' => 'image/jpeg',
  '.gif' => 'image/gif',
  '.webp' => 'image/webp',
  '.svg' => 'image/svg+xml',
  '.bmp' => 'image/bmp',
  '.heic' => 'image/heic',
  '.mp4' || '.m4v' => 'video/mp4',
  '.mov' => 'video/quicktime',
  '.webm' => 'video/webm',
  '.mkv' => 'video/x-matroska',
  _ => 'application/octet-stream',
};

/// Copying a picture works where pasteboard writes images (not Linux, not the web). Tests flip it.
@visibleForTesting
bool? canCopyImageOverride;

bool get canCopyImage =>
    canCopyImageOverride ??
    (!kIsWeb &&
        (Platform.isIOS ||
            Platform.isAndroid ||
            Platform.isMacOS ||
            Platform.isWindows));

/// Opens the share sheet for the file; [origin] anchors it on an iPad.
Future<void> shareBytes(Uint8List bytes, String name, {Rect? origin}) async {
  try {
    await SharePlus.instance.share(
      ShareParams(
        files: [XFile.fromData(bytes, name: name, mimeType: mimeOf(name))],
        fileNameOverrides: [name],
        sharePositionOrigin: origin,
      ),
    );
  } catch (_) {
    // the share sheet failed to open; nothing to undo
  }
}

/// Saves the file to the photo library. Returns why it failed, or null.
Future<String?> saveToPhotos(
  Uint8List bytes,
  String name, {
  bool video = false,
}) async {
  File? tmp;
  try {
    if (video) {
      var ext = _ext(name);
      if (ext.isEmpty) ext = '.mp4';
      tmp = File(
        '${Directory.systemTemp.path}/femme-save-'
        '${DateTime.now().microsecondsSinceEpoch}$ext',
      );
      await tmp.writeAsBytes(bytes, flush: true);
      await Gal.putVideo(tmp.path);
    } else {
      final dot = name.lastIndexOf('.');
      await Gal.putImageBytes(
        bytes,
        name: dot > 0 ? name.substring(0, dot) : name,
      );
    }
    return null;
  } on MissingPluginException {
    return "Save to Photos isn't available on this device";
  } on GalException catch (e) {
    return switch (e.type) {
      GalExceptionType.accessDenied =>
        'no access to Photos: allow it in Settings',
      GalExceptionType.notEnoughSpace => 'not enough space to save it',
      GalExceptionType.notSupportedFormat => "Photos can't take this format",
      GalExceptionType.unexpected => "couldn't save it to Photos",
    };
  } catch (e) {
    return "couldn't save it to Photos: $e";
  } finally {
    try {
      if (tmp != null && await tmp.exists()) await tmp.delete();
    } catch (_) {}
  }
}

/// Puts the picture on the clipboard. Returns why it failed, or null.
Future<String?> copyImage(Uint8List bytes) async {
  if (!canCopyImage) return "can't copy pictures on this device";
  try {
    await Pasteboard.writeImage(bytes);
    return null;
  } on MissingPluginException {
    return "can't copy pictures on this device";
  } catch (e) {
    return "couldn't copy it: $e";
  }
}
