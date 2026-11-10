import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';
import 'package:share_plus/share_plus.dart';

import '../../state/files.dart';

/// The rectangle the share sheet points at on an iPad: the widget of [context].
Rect? shareOrigin(BuildContext context) {
  final box = context.findRenderObject();
  if (box is! RenderBox || !box.hasSize) return null;
  return box.localToGlobal(Offset.zero) & box.size;
}

/// Hands `bytes` to the system share sheet as a file called `name`.
Future<void> shareBytes(BuildContext context, String name, Uint8List bytes) {
  final origin = shareOrigin(context);
  return SharePlus.instance.share(
    ShareParams(
      files: [XFile.fromData(bytes, name: name, mimeType: mimeOf(name))],
      fileNameOverrides: [name],
      sharePositionOrigin: origin,
    ),
  );
}

/// Puts the full path on the box (`cwd/rel`) on the clipboard.
Future<void> copyPath(String cwd, String rel) =>
    Clipboard.setData(ClipboardData(text: fullPath(cwd, rel)));

/// A MIME type for the share sheet, from the name; `application/octet-stream` when unknown.
String mimeOf(String name) {
  final lower = name.toLowerCase();
  final ext = lower.contains('.')
      ? lower.substring(lower.lastIndexOf('.') + 1)
      : '';
  return switch (ext) {
    'png' => 'image/png',
    'jpg' || 'jpeg' => 'image/jpeg',
    'gif' => 'image/gif',
    'webp' => 'image/webp',
    'bmp' => 'image/bmp',
    'svg' => 'image/svg+xml',
    'pdf' => 'application/pdf',
    'mp4' || 'm4v' => 'video/mp4',
    'mov' => 'video/quicktime',
    'webm' => 'video/webm',
    'mp3' => 'audio/mpeg',
    'm4a' => 'audio/mp4',
    'wav' => 'audio/wav',
    'aac' => 'audio/aac',
    'ogg' => 'audio/ogg',
    'flac' => 'audio/flac',
    'json' => 'application/json',
    'html' || 'htm' => 'text/html',
    'md' || 'markdown' => 'text/markdown',
    'csv' => 'text/csv',
    _ =>
      viewerKind(name) == ViewerKind.text
          ? 'text/plain'
          : 'application/octet-stream',
  };
}

/// A one-button alert.
Future<void> showNote(BuildContext context, String title, String body) =>
    showCupertinoDialog<void>(
      context: context,
      builder: (c) => CupertinoAlertDialog(
        title: Text(title),
        content: Text(body),
        actions: [
          CupertinoDialogAction(
            isDefaultAction: true,
            onPressed: () => Navigator.pop(c),
            child: const Text('OK'),
          ),
        ],
      ),
    );

/// "This file is X MiB. Open it?" — true on yes.
Future<bool> askBig(
  BuildContext context,
  int len, {
  String verb = 'Open',
}) async {
  final yes = await showCupertinoDialog<bool>(
    context: context,
    builder: (c) => CupertinoAlertDialog(
      title: Text('This file is ${humanSize(len)}.'),
      content: Text('$verb it?'),
      actions: [
        CupertinoDialogAction(
          onPressed: () => Navigator.pop(c, false),
          child: const Text('Cancel'),
        ),
        CupertinoDialogAction(
          isDefaultAction: true,
          onPressed: () => Navigator.pop(c, true),
          child: Text(verb),
        ),
      ],
    ),
  );
  return yes ?? false;
}

/// The refusal for a file bigger than one read.
String refuseText(int len) =>
    'This file is ${humanSize(len)}; the box sends at most ${humanSize(maxRead)} at once.';

/// Reads `path` for sharing, with the same size rules as opening it, then shares it.
Future<void> shareFile(
  BuildContext context,
  FilesTree tree,
  String path,
  int len,
) async {
  final plan = planOpen(len, cached: tree.cache.contains(path));
  if (plan == OpenPlan.refuse) {
    return showNote(context, 'Too big to share', refuseText(len));
  }
  if (plan == OpenPlan.ask && !await askBig(context, len, verb: 'Share')) {
    return;
  }
  try {
    final bytes = await tree.read(path);
    if (!context.mounted) return;
    await shareBytes(context, baseName(path), bytes);
  } catch (e) {
    if (context.mounted) await showNote(context, 'Could not read it', '$e');
  }
}
