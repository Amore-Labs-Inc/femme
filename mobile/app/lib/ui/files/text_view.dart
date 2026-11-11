import 'dart:convert';
import 'dart:math' as math;

import 'package:flutter/cupertino.dart';

import '../theme.dart';
import 'highlight.dart';

/// The monospace style of the viewers: JetBrains Mono 13 px with the platform's mono behind it.
TextStyle monoStyle(BuildContext context, {Color? color}) => TextStyle(
  fontFamily: 'JetBrains Mono NL',
  fontFamilyFallback: const ['Menlo', 'Roboto Mono', 'monospace'],
  fontSize: 13,
  height: 1.45,
  color: color ?? FemmeColors.inkC.resolveFrom(context),
);

/// The longest line drawn in full; longer lines are cut at this many characters.
const maxLineChars = 2000;

/// Splits decoded text into lines (`\n`, `\r\n`), tabs as 4 spaces; no trailing empty line.
List<String> splitLines(String text) {
  final lines = const LineSplitter().convert(text.replaceAll('\t', '    '));
  return lines.isEmpty ? const [''] : lines;
}

/// Lines with a muted number gutter, one row per line built as it scrolls in, scrolling sideways
/// as one block; selectable across the lines on screen.
class LinesView extends StatefulWidget {
  const LinesView({
    super.key,
    required this.lines,
    this.gutter = true,
    this.runs,
  });

  final List<String> lines;
  final bool gutter;

  /// The lines highlighted ([highlightLines]), one list of runs per line; `null`: plain.
  final List<List<Run>>? runs;

  @override
  State<LinesView> createState() => _LinesViewState();
}

class _LinesViewState extends State<LinesView> {
  late int _longest = _measure();

  int _measure() => widget.lines.fold(0, (m, l) => math.max(m, l.length));

  @override
  void didUpdateWidget(LinesView old) {
    super.didUpdateWidget(old);
    if (!identical(old.lines, widget.lines)) _longest = _measure();
  }

  @override
  Widget build(BuildContext context) {
    final style = monoStyle(context);
    final gutterStyle = monoStyle(
      context,
      color: FemmeColors.mutedC.resolveFrom(context),
    );
    final probe = TextPainter(
      text: TextSpan(text: 'M', style: style),
      textDirection: TextDirection.ltr,
    )..layout();
    final charW = probe.width;
    final lineH = probe.height;
    probe.dispose();
    final dark = CupertinoTheme.brightnessOf(context) == Brightness.dark;
    final runs = widget.runs;
    final digits = widget.lines.length.toString().length;
    final gutterW = widget.gutter ? (digits + 1) * charW + 12 : 0.0;
    final textW = math.min(_longest, maxLineChars) * charW + 24;

    return LayoutBuilder(
      builder: (context, box) {
        final width = math.max(box.maxWidth, gutterW + textW);
        return SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          child: SizedBox(
            width: width,
            height: box.maxHeight,
            child: SelectableRegion(
              selectionControls: cupertinoTextSelectionHandleControls,
              contextMenuBuilder: (context, state) =>
                  CupertinoAdaptiveTextSelectionToolbar.buttonItems(
                    anchors: state.contextMenuAnchors,
                    buttonItems: state.contextMenuButtonItems,
                  ),
              child: ListView.builder(
                padding: EdgeInsets.only(
                  top: 8,
                  bottom: 8 + MediaQuery.paddingOf(context).bottom,
                ),
                itemCount: widget.lines.length,
                itemExtent: lineH,
                itemBuilder: (context, i) {
                  final line = widget.lines[i];
                  return Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      if (widget.gutter)
                        SelectionContainer.disabled(
                          child: Container(
                            width: gutterW,
                            padding: const EdgeInsets.only(right: 8),
                            alignment: Alignment.topRight,
                            child: Text('${i + 1}', style: gutterStyle),
                          ),
                        ),
                      Padding(
                        padding: const EdgeInsets.only(left: 4),
                        child: runs != null && i < runs.length
                            ? Text.rich(
                                TextSpan(
                                  children: _spans(runs[i], dark),
                                  style: style,
                                ),
                                maxLines: 1,
                                softWrap: false,
                              )
                            : Text(
                                line.length > maxLineChars
                                    ? '${line.substring(0, maxLineChars)}…'
                                    : line,
                                style: style,
                                maxLines: 1,
                                softWrap: false,
                              ),
                      ),
                    ],
                  );
                },
              ),
            ),
          ),
        );
      },
    );
  }
}

/// One line's runs as spans, cut at [maxLineChars] like the plain lines.
List<TextSpan> _spans(List<Run> line, bool dark) {
  final out = <TextSpan>[];
  var left = maxLineChars;
  for (final r in line) {
    if (left <= 0) break;
    final cut = r.text.length > left;
    out.add(
      TextSpan(
        text: cut ? '${r.text.substring(0, left)}…' : r.text,
        style: r.tok == Tok.plain
            ? null
            : TextStyle(color: tokColor(r.tok, dark: dark)),
      ),
    );
    left -= r.text.length;
  }
  return out;
}
