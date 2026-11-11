import 'dart:convert';

import 'package:flutter/cupertino.dart';
import 'package:flutter/foundation.dart';
import 'package:flutter/services.dart';

import '../../state/files.dart';
import '../chat/markdown.dart';
import '../theme.dart';
import 'hex.dart';
import 'highlight.dart';
import 'media.dart';
import 'pdf.dart';
import 'share.dart';
import 'text_view.dart';

/// A text file ready to draw: its lines and, for code, their highlighted runs.
typedef TextDoc = ({List<String> lines, List<List<Run>>? runs});

/// Decodes, splits and highlights; off the UI thread when the file is big.
Future<TextDoc> textDocOf(Uint8List bytes, String? language) =>
    bytes.length > (256 << 10)
    ? compute(_prepare, (bytes: bytes, language: language))
    : Future.value(_prepare((bytes: bytes, language: language)));

TextDoc _prepare(({Uint8List bytes, String? language}) a) {
  final lines = splitLines(utf8.decode(a.bytes, allowMalformed: true));
  return (lines: lines, runs: highlightLines(lines, a.language));
}

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
  TextDoc? _text;
  Object? _error;

  /// Markdown shown as its raw text.
  bool _raw = false;

  String get _name => baseName(widget.path);

  @override
  void initState() {
    super.initState();
    final cached = widget.tree.cache.contains(widget.path);
    final reading = widget.tree.reading(widget.path);
    // Below the lowest threshold nothing depends on the network: plan now.
    if (cached ||
        reading ||
        widget.len <= askAboveCellular ||
        widget.len > maxRead) {
      _plan(askAboveCellular);
    } else {
      askAboveNow().then((above) {
        if (mounted) _plan(above);
      });
    }
  }

  /// The size rules with the ask threshold of the network the phone is on.
  void _plan(int askAbove) {
    final plan = planOpen(
      widget.len,
      cached: widget.tree.cache.contains(widget.path),
      reading: widget.tree.reading(widget.path),
      askAbove: askAbove,
    );
    switch (plan) {
      case OpenPlan.refuse:
        setState(() => _phase = _Phase.refused);
      case OpenPlan.ask:
        setState(() => _phase = _Phase.asking);
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
      final text = kind == ViewerKind.text
          ? await textDocOf(bytes, languageFor(_name))
          : null;
      if (!mounted) return;
      setState(() {
        _bytes = bytes;
        _kind = kind;
        _text = text;
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

  /// A markdown file shown as text: rendered, with the switch to raw.
  bool get _markdown =>
      _phase == _Phase.shown && _kind == ViewerKind.text && isMarkdown(_name);

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
        trailing: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            if (_markdown)
              CupertinoButton(
                key: const Key('md-toggle'),
                padding: EdgeInsets.zero,
                minimumSize: const Size(44, 44),
                onPressed: () {
                  HapticFeedback.selectionClick();
                  setState(() => _raw = !_raw);
                },
                child: Icon(
                  _raw
                      ? CupertinoIcons.doc_richtext
                      : CupertinoIcons.doc_plaintext,
                  semanticLabel: _raw ? 'Rendered' : 'Raw text',
                ),
              ),
            Builder(
              builder: (bctx) => CupertinoButton(
                padding: EdgeInsets.zero,
                minimumSize: const Size(44, 44),
                onPressed: bytes == null
                    ? null
                    : () {
                        HapticFeedback.selectionClick();
                        shareBytes(bctx, _name, bytes);
                      },
                child: const Icon(CupertinoIcons.share, semanticLabel: 'share'),
              ),
            ),
          ],
        ),
      ),
      child: SafeArea(
        bottom: !_dark && _kind != ViewerKind.text && !_markdown,
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
    return fileBody(
      name: _name,
      kind: _kind!,
      bytes: _bytes!,
      text: _text,
      raw: _raw,
    );
  }
}

/// The view for a file's bytes once read: [kind] from [shownKind], [text] from [textDocOf] for
/// text. Markdown renders unless [raw].
Widget fileBody({
  required String name,
  required ViewerKind kind,
  required Uint8List bytes,
  TextDoc? text,
  bool raw = false,
}) => switch (kind) {
  ViewerKind.image => ImageView(bytes: bytes),
  ViewerKind.svg => SvgView(bytes: bytes),
  ViewerKind.text when isMarkdown(name) && !raw => MarkdownView(
    text: utf8.decode(bytes, allowMalformed: true),
  ),
  ViewerKind.text => LinesView(lines: text!.lines, runs: text.runs),
  ViewerKind.video => MediaView(name: name, bytes: bytes),
  ViewerKind.audio => MediaView(name: name, bytes: bytes, audio: true),
  ViewerKind.pdf => PdfPages(name: name, bytes: bytes),
  ViewerKind.binary => HexView(
    bytes: bytes,
    note: viewerKind(name) == ViewerKind.text && bytes.length > textMax
        ? 'Text over ${humanSize(textMax)}: first ${humanSize(headBytes)} as hex.'
        : null,
  ),
};

/// A markdown file rendered with the chat's [Md], scrolling, selectable.
class MarkdownView extends StatelessWidget {
  const MarkdownView({super.key, required this.text});
  final String text;

  @override
  Widget build(BuildContext context) => SelectableRegion(
    selectionControls: cupertinoTextSelectionHandleControls,
    contextMenuBuilder: (context, state) =>
        CupertinoAdaptiveTextSelectionToolbar.buttonItems(
          anchors: state.contextMenuAnchors,
          buttonItems: state.contextMenuButtonItems,
        ),
    child: SingleChildScrollView(
      padding: EdgeInsets.fromLTRB(
        16,
        12,
        16,
        16 + MediaQuery.paddingOf(context).bottom,
      ),
      child: Md(text),
    ),
  );
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

/// The page's action: a filled accent button with 12 pt corners, 44 pt tall.
class _FlatButton extends StatelessWidget {
  const _FlatButton({required this.label, this.onPressed});
  final String label;
  final VoidCallback? onPressed;

  @override
  Widget build(BuildContext context) => CupertinoButton.filled(
    minimumSize: const Size(120, 44),
    borderRadius: const BorderRadius.all(Radius.circular(12)),
    onPressed: onPressed == null
        ? null
        : () {
            HapticFeedback.selectionClick();
            onPressed!();
          },
    child: Text(label),
  );
}
