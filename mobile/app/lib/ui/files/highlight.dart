import 'package:flutter/cupertino.dart';
// `highlight` comes with `flutter_highlight` (pubspec); its widget is Material, the parser is not.
// ignore: depend_on_referenced_packages
import 'package:highlight/highlight.dart' show highlight, Node;

import '../theme.dart';

// Syntax highlighting for the text viewer. `highlight` parses the source into highlight.js class
// names; this file folds them into the desktop's few token classes (`viewer/highlight.rs`) and
// paints those in FEMME colours, so no highlight.js theme is involved.

/// What a stretch of source is, as far as the colours care (desktop `highlight.rs` `Class`).
enum Tok { plain, comment, keyword, str, number, type, function, invalid }

/// Sources bigger than this are shown plain: parsing them is not worth the wait.
const highlightMax = 1 << 20;

/// Lines longer than this stay plain, like the desktop's `MAX_PARSE_LINE`.
const maxParseLine = 4096;

/// A run of one line in one class.
typedef Run = ({String text, Tok tok});

const _classes = <String, Tok>{
  'comment': Tok.comment,
  'quote': Tok.comment,
  'doctag': Tok.comment,
  'keyword': Tok.keyword,
  'selector-tag': Tok.keyword,
  'name': Tok.keyword,
  'section': Tok.keyword,
  'meta-keyword': Tok.keyword,
  'builder-name': Tok.keyword,
  'string': Tok.str,
  'regexp': Tok.str,
  'meta-string': Tok.str,
  'addition': Tok.str,
  'code': Tok.str,
  'number': Tok.number,
  'literal': Tok.number,
  'symbol': Tok.number,
  'bullet': Tok.number,
  'template-variable': Tok.number,
  'type': Tok.type,
  'class': Tok.type,
  'built_in': Tok.type,
  'attr': Tok.type,
  'attribute': Tok.type,
  'selector-class': Tok.type,
  'selector-id': Tok.type,
  'selector-attr': Tok.type,
  'selector-pseudo': Tok.type,
  'meta': Tok.type,
  'title': Tok.function,
  'function': Tok.function,
  'link': Tok.function,
  'deletion': Tok.invalid,
  'params': Tok.plain,
  'subst': Tok.plain,
};

/// The class of a highlight.js class name; an unknown one keeps its parent's.
Tok classOf(String? className, Tok parent) =>
    className == null ? parent : (_classes[className] ?? parent);

/// `lines` (as [splitLines] cut them) as runs per line, parsed with `language`. `null` when the
/// grammar is unknown, the source too big, or the parse fails: the caller shows plain lines.
List<List<Run>>? highlightLines(List<String> lines, String? language) {
  if (language == null) return null;
  var size = 0;
  for (final l in lines) {
    size += l.length + 1;
    if (size > highlightMax) return null;
  }
  final out = <List<Run>>[[]];
  final long = <int>{
    for (var i = 0; i < lines.length; i++)
      if (lines[i].length > maxParseLine) i,
  };
  void add(String text, Tok tok) {
    final parts = text.split('\n');
    for (var i = 0; i < parts.length; i++) {
      if (i > 0) out.add([]);
      if (parts[i].isEmpty) continue;
      final line = out.last;
      if (line.isNotEmpty && line.last.tok == tok) {
        line[line.length - 1] = (text: line.last.text + parts[i], tok: tok);
      } else {
        line.add((text: parts[i], tok: tok));
      }
    }
  }

  void walk(Node node, Tok parent) {
    final tok = classOf(node.className, parent);
    final v = node.value;
    if (v != null) add(v, tok);
    for (final c in node.children ?? const <Node>[]) {
      walk(c, tok);
    }
  }

  try {
    final source = [
      for (var i = 0; i < lines.length; i++) long.contains(i) ? '' : lines[i],
    ].join('\n');
    final nodes = highlight.parse(source, language: language).nodes;
    for (final n in nodes ?? const <Node>[]) {
      walk(n, Tok.plain);
    }
  } catch (_) {
    return null;
  }
  // The parse keeps every character, so the lines line up; a grammar that did not is dropped.
  while (out.length < lines.length) {
    out.add([]);
  }
  if (out.length != lines.length) return null;
  for (final i in long) {
    out[i] = [(text: lines[i], tok: Tok.plain)];
  }
  return out;
}

/// `a` moved `t` of the way to `b`.
Color _mix(Color a, Color b, double t) => Color.lerp(a, b, t)!;

/// The colour of each class for one brightness, as on the desktop: light mixes the status colours
/// toward ink until they read on paper; dark uses them as they are.
Color tokColor(Tok tok, {required bool dark}) {
  if (dark) {
    return switch (tok) {
      Tok.plain => FemmeColors.darkInk,
      Tok.comment => FemmeColors.darkMuted,
      Tok.keyword => const Color(0xFFD45BB6),
      Tok.str => const Color(0xFF2FD07A),
      Tok.number => _mix(const Color(0xFFFF6B70), FemmeColors.darkInk, 0.25),
      Tok.type => const Color(0xFFF5C24B),
      Tok.function => const Color(0xFF2CD3C4),
      Tok.invalid => const Color(0xFFFF6B70),
    };
  }
  const ink = FemmeColors.ink;
  return switch (tok) {
    Tok.plain => ink,
    Tok.comment => FemmeColors.muted,
    Tok.keyword => _mix(FemmeColors.accent, ink, 0.30),
    Tok.str => _mix(FemmeColors.work, ink, 0.35),
    Tok.number => _mix(FemmeColors.bad, ink, 0.25),
    Tok.type => _mix(FemmeColors.pause, ink, 0.50),
    Tok.function => _mix(FemmeColors.pr, ink, 0.40),
    Tok.invalid => FemmeColors.bad,
  };
}
