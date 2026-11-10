import 'dart:convert';
import 'dart:math' as math;

import 'package:flutter/cupertino.dart';

import '../theme.dart';

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
  const LinesView({super.key, required this.lines, this.gutter = true});

  final List<String> lines;
  final bool gutter;

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
                        child: Text(
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
