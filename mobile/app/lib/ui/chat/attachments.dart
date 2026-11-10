/// Files picked for the next prompt: picked from the camera, Photos or Files, uploaded at once
/// with `BlobPut`, shown as chips (uploading / size / failed · retry / remove).
library;

import 'dart:typed_data';

import 'package:file_picker/file_picker.dart';
import 'package:flutter/cupertino.dart';
import 'package:image_picker/image_picker.dart';

import '../../net/link.dart';
import '../../net/rpc.dart';
import '../../state/chat.dart';
import '../theme.dart';
import 'attached.dart';
import 'look.dart';

const maxFileBytes = 8 * 1024 * 1024;
const maxFiles = 16;

enum UploadState { uploading, ready, failed }

class DraftFile {
  DraftFile(this.name, this.bytes);
  final String name;
  final Uint8List bytes;
  UploadState state = UploadState.uploading;
  List<int>? hash;
  String? error;
}

/// The files of the prompt being written.
class DraftFiles extends ChangeNotifier {
  DraftFiles(this.link);
  final Requester link;
  final files = <DraftFile>[];
  bool _disposed = false;

  @override
  void dispose() {
    _disposed = true;
    super.dispose();
  }

  bool get uploading => files.any((f) => f.state == UploadState.uploading);
  bool get failed => files.any((f) => f.state == UploadState.failed);
  bool get isEmpty => files.isEmpty;

  List<Attachment> get ready => [
    for (final f in files)
      if (f.state == UploadState.ready) Attachment(f.name, f.hash!),
  ];

  /// `name`, else `name (2).ext`, … when taken.
  String _unique(String name) {
    final taken = {for (final f in files) f.name};
    if (!taken.contains(name)) return name;
    final dot = name.lastIndexOf('.');
    final stem = dot <= 0 ? name : name.substring(0, dot);
    final ext = dot <= 0 ? '' : name.substring(dot);
    for (var n = 2; ; n++) {
      final c = '$stem ($n)$ext';
      if (!taken.contains(c)) return c;
    }
  }

  /// Adds a file and uploads it. Returns why it was refused, or null.
  String? add(String name, Uint8List bytes) {
    if (files.length >= maxFiles) return 'at most $maxFiles files per message';
    if (bytes.isEmpty) return '$name is empty';
    if (bytes.length > maxFileBytes) {
      return '$name is ${byteSize(bytes.length)}: the limit is 8 MB';
    }
    final f = DraftFile(_unique(name), bytes);
    files.add(f);
    _changed();
    _upload(f);
    return null;
  }

  void remove(DraftFile f) {
    files.remove(f);
    _changed();
  }

  void retry(DraftFile f) {
    f
      ..state = UploadState.uploading
      ..error = null;
    _changed();
    _upload(f);
  }

  void clear() {
    files.clear();
    _changed();
  }

  Future<void> _upload(DraftFile f) async {
    try {
      final r = await link.req(
        Req.blobPut(f.name, f.bytes),
        timeout: const Duration(minutes: 2),
      );
      f
        ..hash = r.blobHash
        ..state = UploadState.ready;
    } catch (e) {
      f
        ..state = UploadState.failed
        ..error = errorText(e);
    }
    _changed();
  }

  void _changed() {
    if (!_disposed) notifyListeners();
  }
}

/// Asks where to pick from, then adds what was picked. Returns refusal notes.
Future<List<String>> pickInto(BuildContext context, DraftFiles into) async {
  final source = await showCupertinoModalPopup<String>(
    context: context,
    builder: (ctx) => CupertinoActionSheet(
      actions: [
        CupertinoActionSheetAction(
          onPressed: () => Navigator.pop(ctx, 'camera'),
          child: const Text('Take Photo'),
        ),
        CupertinoActionSheetAction(
          onPressed: () => Navigator.pop(ctx, 'photos'),
          child: const Text('Photo Library'),
        ),
        CupertinoActionSheetAction(
          onPressed: () => Navigator.pop(ctx, 'files'),
          child: const Text('Files'),
        ),
      ],
      cancelButton: CupertinoActionSheetAction(
        onPressed: () => Navigator.pop(ctx),
        child: const Text('Cancel'),
      ),
    ),
  );
  final notes = <String>[];
  try {
    final picked = <(String, Uint8List)>[];
    switch (source) {
      case 'camera':
        final x = await ImagePicker().pickImage(source: ImageSource.camera);
        if (x != null) picked.add((x.name, await x.readAsBytes()));
      case 'photos':
        for (final x in await ImagePicker().pickMultipleMedia()) {
          picked.add((x.name, await x.readAsBytes()));
        }
      case 'files':
        for (final f in await FilePicker.pickFiles()) {
          picked.add((f.name, await f.readAsBytes()));
        }
      default:
        return notes;
    }
    for (final (name, bytes) in picked) {
      final why = into.add(name, bytes);
      if (why != null) notes.add(why);
    }
  } catch (e) {
    notes.add('could not pick: $e');
  }
  return notes;
}

/// The chips of the files picked so far.
class FileChips extends StatelessWidget {
  const FileChips(this.files, {super.key});
  final DraftFiles files;

  @override
  Widget build(BuildContext context) => ListenableBuilder(
    listenable: files,
    builder: (context, _) {
      if (files.isEmpty) return const SizedBox.shrink();
      return SizedBox(
        height: 64,
        child: ListView(
          scrollDirection: Axis.horizontal,
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 6),
          children: [for (final f in files.files) _FileChip(f, files)],
        ),
      );
    },
  );
}

class _FileChip extends StatelessWidget {
  const _FileChip(this.f, this.files);
  final DraftFile f;
  final DraftFiles files;

  @override
  Widget build(BuildContext context) {
    final failed = f.state == UploadState.failed;
    final status = switch (f.state) {
      UploadState.uploading => 'uploading…',
      UploadState.ready => byteSize(f.bytes.length),
      UploadState.failed => 'failed · retry',
    };
    return GestureDetector(
      onTap: failed ? () => files.retry(f) : null,
      child: Container(
        margin: const EdgeInsets.only(right: 8),
        padding: const EdgeInsets.only(left: 4),
        decoration: BoxDecoration(
          color: paper(context),
          border: Border.all(
            color: failed ? FemmeColors.bad : ink(context),
            width: 1.2,
          ),
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (isPicture(f.name) && !isSvg(f.name))
              Image.memory(f.bytes, width: 40, height: 40, fit: BoxFit.cover)
            else
              const Icon(CupertinoIcons.doc, size: 28),
            const SizedBox(width: 6),
            ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 140),
              child: Column(
                mainAxisAlignment: MainAxisAlignment.center,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    f.name,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: mono(context, size: 12),
                  ),
                  Row(
                    children: [
                      if (f.state == UploadState.uploading)
                        const Padding(
                          padding: EdgeInsets.only(right: 4),
                          child: CupertinoActivityIndicator(radius: 6),
                        ),
                      Flexible(
                        child: Text(
                          failed && f.error != null
                              ? '${f.error} · retry'
                              : status,
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: mono(
                            context,
                            size: 11,
                            color: failed ? FemmeColors.bad : muted(context),
                          ),
                        ),
                      ),
                    ],
                  ),
                ],
              ),
            ),
            CupertinoButton(
              padding: EdgeInsets.zero,
              minimumSize: const Size(36, 44),
              onPressed: () => files.remove(f),
              child: Icon(
                CupertinoIcons.xmark,
                size: 16,
                color: muted(context),
              ),
            ),
          ],
        ),
      ),
    );
  }
}
