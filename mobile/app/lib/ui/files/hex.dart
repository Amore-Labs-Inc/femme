import 'package:flutter/cupertino.dart';

import '../../state/files.dart';
import 'text_view.dart';

/// One hex dump row: `00000010  48 65 6c 6c 6f 20 77 6f  72 6c 64 0a ...  |Hello world.|`.
String hexRow(List<int> bytes, int offset) {
  final end = (offset + 16).clamp(0, bytes.length);
  final row = bytes.sublist(offset, end);
  final hex = StringBuffer();
  for (var i = 0; i < 16; i++) {
    if (i == 8) hex.write(' ');
    hex.write(i < row.length ? row[i].toRadixString(16).padLeft(2, '0') : '  ');
    if (i < 15) hex.write(' ');
  }
  final ascii = row
      .map((b) => b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : '.')
      .join();
  return '${offset.toRadixString(16).padLeft(8, '0')}  $hex  |$ascii|';
}

/// The hex dump of the first [headBytes] of `bytes`.
List<String> hexLines(List<int> bytes) {
  final head = bytes.length > headBytes ? bytes.sublist(0, headBytes) : bytes;
  return [for (var o = 0; o < head.length; o += 16) hexRow(head, o)];
}

/// The first 4 KiB as a hex dump, with a line saying how much more there is.
class HexView extends StatelessWidget {
  const HexView({super.key, required this.bytes, this.note});

  final List<int> bytes;

  /// Why this is a hex dump (e.g. "Text over 8 MiB").
  final String? note;

  @override
  Widget build(BuildContext context) {
    final rest = bytes.length - headBytes;
    final lines = [
      ?note,
      if (bytes.isEmpty) '(empty file)',
      ...hexLines(bytes),
      if (rest > 0) '… ${humanSize(rest)} more; share to see it all',
    ];
    return LinesView(lines: lines, gutter: false);
  }
}
