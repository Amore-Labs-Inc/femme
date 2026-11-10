import 'dart:convert';

import 'package:flutter/cupertino.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import '../../state/files.dart';
import '../theme.dart';
import 'hex.dart';
import 'media.dart';
import 'share.dart';
import 'text_view.dart';

/// Decodes and splits off the UI thread when the file is big.
Future<List<String>> _linesOf(Uint8List bytes) => bytes.length > (256 << 10)
    ? compute(_decode, bytes)
    : Future.value(_decode(bytes));

List<String> _decode(Uint8List bytes) =>
    splitLines(utf8.decode(bytes, allowMalformed: true));

/// A file of the session folder on its own page: size rules first, then the read, then the
/// viewer its kind asks for. Share in the bar once the bytes are in.
class FileViewerPage extends StatefulWidget {
  const FileViewerPage({
    super.key,
    required this.tree,
    required this.path,
    required this.len,
  });

  final FilesTree tree;
  final String path;

  /// The size as listed.
  final int len;

  @override
  State<FileViewerPage> createState() => _FileViewerPageState();
}

enum _Phase { asking, refused, loading, shown, failed }

class _FileViewerPageState extends State<FileViewerPage> {
  _Phase _phase = _Phase.loading;
  Uint8List? _bytes;
  ViewerKind? _kind;
  List<String>? _lines;
  Object? _error;

  String get _name => baseName(widget.path);

  @override
  void initState() {
    super.initState();
    final plan = planOpen(
      widget.len,
      cached: widget.tree.cache.contains(widget.path),
      reading: widget.tree.reading(widget.path),
    );
    switch (plan) {
      case OpenPlan.refuse:
        _phase = _Phase.refused;
      case OpenPlan.ask:
        _phase = _Phase.asking;
        WidgetsBinding.instance.addPostFrameCallback((_) => _ask());
      case OpenPlan.show || OpenPlan.wait || OpenPlan.read:
        _load();
    }
  }

  Future<void> _ask() async {
    if (!mounted) return;
    if (await askBig(context, widget.len)) {
      _load();
    } else if (mounted) {
      Navigator.of(context).maybePop();
    }
  }

  Future<void> _load() async {
    setState(() {
      _phase = _Phase.loading;
      _error = null;
    });
    try {
      final bytes = await widget.tree.read(widget.path);
      final kind = shownKind(_name, bytes);
      final lines = kind == ViewerKind.text ? await _linesOf(bytes) : null;
      if (!mounted) return;
      setState(() {
        _bytes = bytes;
        _kind = kind;
        _lines = lines;
        _phase = _Phase.shown;
      });
    } catch (e) {
      if (mounted) {
        setState(() {
          _error = e;
          _phase = _Phase.failed;
        });
      }
    }
  }

  bool get _dark =>
      _phase == _Phase.shown &&
      (_kind == ViewerKind.image ||
          _kind == ViewerKind.video ||
          _kind == ViewerKind.audio);

  @override
  Widget build(BuildContext context) {
    final bytes = _bytes;
    return CupertinoPageScaffold(
      backgroundColor: _dark ? backdrop : null,
      navigationBar: CupertinoNavigationBar(
        middle: Text(_name, overflow: TextOverflow.ellipsis),
        trailing: Builder(
          builder: (bctx) => CupertinoButton(
            padding: EdgeInsets.zero,
            minimumSize: const Size(44, 44),
            onPressed: bytes == null
                ? null
                : () {
                    HapticFeedback.selectionClick();
                    shareBytes(bctx, _name, bytes);
                  },
            child: const Icon(CupertinoIcons.share),
          ),
        ),
      ),
      child: SafeArea(
        bottom: !_dark && _kind != ViewerKind.text,
        child: _body(context),
      ),
    );
  }

  Widget _body(BuildContext context) {
    switch (_phase) {
      case _Phase.asking:
        return _Note(
          text: 'This file is ${humanSize(widget.len)}.',
          action: 'Open',
          onAction: _load,
        );
      case _Phase.refused:
        return _Note(text: refuseText(widget.len));
      case _Phase.loading:
        return const Center(child: CupertinoActivityIndicator());
      case _Phase.failed:
        return _Note(
          text: 'Could not read it.\n$_error',
          action: 'Try again',
          onAction: _load,
        );
      case _Phase.shown:
        break;
    }
    final bytes = _bytes!;
    return switch (_kind!) {
      ViewerKind.image => ImageView(bytes: bytes),
      ViewerKind.text => LinesView(lines: _lines!),
      ViewerKind.video => MediaView(name: _name, bytes: bytes),
      ViewerKind.audio => MediaView(name: _name, bytes: bytes, audio: true),
      ViewerKind.pdf => Builder(
        builder: (bctx) => _Note(
          text: 'PDF: share to open it in Files or Books.',
          action: 'Share',
          onAction: () => shareBytes(bctx, _name, bytes),
        ),
      ),
      ViewerKind.binary => HexView(
        bytes: bytes,
        note: viewerKind(_name) == ViewerKind.text && bytes.length > textMax
            ? 'Text over ${humanSize(textMax)}: first ${humanSize(headBytes)} as hex.'
            : null,
      ),
    };
  }
}

/// A centred line, with a button under it.
class _Note extends StatelessWidget {
  const _Note({required this.text, this.action, this.onAction});
  final String text;
  final String? action;
  final VoidCallback? onAction;

  @override
  Widget build(BuildContext context) => Center(
    child: Padding(
      padding: const EdgeInsets.all(24),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          Text(
            text,
            textAlign: TextAlign.center,
            style: TextStyle(color: FemmeColors.mutedC.resolveFrom(context)),
          ),
          if (action != null) ...[
            const SizedBox(height: 16),
            _FlatButton(label: action!, onPressed: onAction),
          ],
        ],
      ),
    ),
  );
}

/// The design's button: ink border, no radius, a hard 2 px shadow.
class _FlatButton extends StatelessWidget {
  const _FlatButton({required this.label, this.onPressed});
  final String label;
  final VoidCallback? onPressed;

  @override
  Widget build(BuildContext context) {
    final ink = FemmeColors.inkC.resolveFrom(context);
    return GestureDetector(
      onTap: onPressed == null
          ? null
          : () {
              HapticFeedback.selectionClick();
              onPressed!();
            },
      child: Container(
        constraints: const BoxConstraints(minHeight: 44, minWidth: 120),
        padding: const EdgeInsets.symmetric(horizontal: 20),
        alignment: Alignment.center,
        decoration: BoxDecoration(
          color: FemmeColors.paperC.resolveFrom(context),
          border: Border.all(color: ink, width: 1.2),
          boxShadow: [BoxShadow(color: ink, offset: const Offset(2, 2))],
        ),
        child: Text(label, style: TextStyle(color: ink, fontSize: 16)),
      ),
    );
  }
}
