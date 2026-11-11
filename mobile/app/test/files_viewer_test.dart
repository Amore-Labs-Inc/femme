import 'dart:typed_data';

import 'package:femme_mobile/state/files.dart';
import 'package:femme_mobile/ui/chat/markdown.dart';
import 'package:femme_mobile/ui/files/highlight.dart';
import 'package:femme_mobile/ui/files/media.dart';
import 'package:femme_mobile/ui/files/pdf.dart';
import 'package:femme_mobile/ui/files/text_view.dart';
import 'package:femme_mobile/ui/files/viewer.dart';
import 'package:femme_mobile/net/rpc.dart';
import 'package:flutter/cupertino.dart';
import 'package:flutter_test/flutter_test.dart';

import 'files_fake.dart';

Uint8List bytesOf(String s) => Uint8List.fromList(s.codeUnits);

/// Every run of every line glued back: what the highlighter must not change.
List<String> glued(List<List<Run>> runs) => [
  for (final l in runs) l.map((r) => r.text).join(),
];

void main() {
  group('languageFor', () {
    test('by extension and by name', () {
      final cases = {
        'main.rs': 'rust',
        'App.TSX': 'typescript',
        'x.py': 'python',
        'lib.dart': 'dart',
        'a.json': 'json',
        'ci.yml': 'yaml',
        'Cargo.toml': 'ini',
        'index.html': 'xml',
        'logo.svg': 'xml',
        'run.sh': 'bash',
        'fix.patch': 'diff',
        'README.md': 'markdown',
        'Makefile': 'makefile',
        'Dockerfile': 'dockerfile',
        'CMakeLists.txt': 'cmake',
        '.bashrc': 'bash',
      };
      cases.forEach((name, lang) {
        expect(languageFor(name), lang, reason: name);
      });
    });
    test('plain text and unknown kinds have none', () {
      for (final n in ['notes.txt', 'LICENSE', 'x.log', 'noext', 'a.zig']) {
        expect(languageFor(n), isNull, reason: n);
      }
    });
    test('markdown by extension', () {
      expect(isMarkdown('README.md'), isTrue);
      expect(isMarkdown('doc.MARKDOWN'), isTrue);
      expect(isMarkdown('notes.txt'), isFalse);
      expect(isMarkdown('md'), isFalse);
    });
  });

  group('highlightLines', () {
    test('keeps every line and character, colours keywords and strings', () {
      final lines = splitLines('fn main() {\n    // hi\n    let s = "x";\n}\n');
      final runs = highlightLines(lines, 'rust')!;
      expect(glued(runs), lines);
      expect(
        runs[0].any((r) => r.tok == Tok.keyword && r.text == 'fn'),
        isTrue,
      );
      expect(runs[1].any((r) => r.tok == Tok.comment), isTrue);
      expect(runs[2].any((r) => r.tok == Tok.str && r.text == '"x"'), isTrue);
    });
    test('a comment over several lines stays a comment on each', () {
      final lines = splitLines('/* a\nb */\nint x;');
      final runs = highlightLines(lines, 'cpp')!;
      expect(glued(runs), lines);
      expect(runs[0].single.tok, Tok.comment);
      expect(runs[1].single.tok, Tok.comment);
    });
    test('none for no grammar, a too-big source; long lines stay plain', () {
      expect(highlightLines(['a'], null), isNull);
      expect(highlightLines(['x' * (highlightMax + 1)], 'rust'), isNull);
      final long = 'let a = 1;' * 500;
      final runs = highlightLines(['let b = 2;', long], 'rust')!;
      expect(runs[1], [(text: long, tok: Tok.plain)]);
      expect(runs[0].any((r) => r.tok == Tok.keyword), isTrue);
    });
    test('the colours follow the theme', () {
      expect(tokColor(Tok.plain, dark: false), const Color(0xFF1E1E1E));
      expect(tokColor(Tok.plain, dark: true), const Color(0xFFECECEC));
      expect(
        tokColor(Tok.keyword, dark: false),
        isNot(tokColor(Tok.keyword, dark: true)),
      );
    });
  });

  test('page label', () {
    expect(pageLabel(3, 12), '3 / 12');
  });

  group('fileBody picks the view', () {
    test('svg, pdf, markdown, code', () {
      final b = bytesOf('x');
      expect(
        fileBody(name: 'logo.svg', kind: ViewerKind.svg, bytes: b),
        isA<SvgView>(),
      );
      expect(
        fileBody(name: 'doc.pdf', kind: ViewerKind.pdf, bytes: b),
        isA<PdfPages>(),
      );
      expect(
        fileBody(name: 'README.md', kind: ViewerKind.text, bytes: b),
        isA<MarkdownView>(),
      );
      const doc = (lines: ['x'], runs: null);
      expect(
        fileBody(
          name: 'README.md',
          kind: ViewerKind.text,
          bytes: b,
          text: doc,
          raw: true,
        ),
        isA<LinesView>(),
      );
      expect(
        fileBody(name: 'a.rs', kind: ViewerKind.text, bytes: b, text: doc),
        isA<LinesView>(),
      );
    });
    test('svg is its own kind once read', () {
      expect(shownKind('logo.svg', bytesOf('<svg/>')), ViewerKind.svg);
    });
  });

  Future<void> open(WidgetTester t, String name, String body) async {
    final box = FakeBox(
      dirs: {
        '': [FsEntry(name, false, body.length)],
      },
      files: {name: body.codeUnits},
    );
    final tree = FilesTree(requester: box, sid: 's1');
    await t.pumpWidget(
      CupertinoApp(
        home: FileViewerPage(tree: tree, path: name, len: body.length),
      ),
    );
    await t.pumpAndSettle();
  }

  testWidgets('code shows highlighted lines with their numbers', (t) async {
    await open(t, 'main.rs', 'fn main() {}\n// two\n');
    expect(find.text('1'), findsOneWidget);
    expect(find.text('2'), findsOneWidget);
    final view = t.widget<LinesView>(find.byType(LinesView));
    expect(view.runs, isNotNull);
    expect(find.byType(SelectableRegion), findsOneWidget);
  });

  testWidgets('markdown renders, the switch shows the raw text and back', (
    t,
  ) async {
    await open(t, 'README.md', '# Title\n\nsome *words*\n');
    expect(find.byType(Md), findsOneWidget);
    expect(find.byType(LinesView), findsNothing);
    expect(find.byType(SelectableRegion), findsOneWidget);

    await t.tap(find.byKey(const Key('md-toggle')));
    await t.pumpAndSettle();
    expect(find.byType(Md), findsNothing);
    expect(find.byType(LinesView), findsOneWidget);
    expect(find.text('# Title'), findsOneWidget);

    await t.tap(find.byKey(const Key('md-toggle')));
    await t.pumpAndSettle();
    expect(find.byType(Md), findsOneWidget);
  });

  testWidgets('plain text has no markdown switch', (t) async {
    await open(t, 'notes.txt', 'hello');
    expect(find.byKey(const Key('md-toggle')), findsNothing);
    expect(find.text('hello'), findsOneWidget);
    expect(t.widget<LinesView>(find.byType(LinesView)).runs, isNull);
  });

  testWidgets('the Share button is named for VoiceOver', (t) async {
    await open(t, 'notes.txt', 'hello');
    expect(find.bySemanticsLabel('share'), findsOneWidget);
  });
}
