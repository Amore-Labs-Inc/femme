import 'package:femme_mobile/net/term_models.dart';
import 'package:femme_mobile/ui/terminal/keys.dart';
import 'package:femme_mobile/ui/terminal/painter.dart';
import 'package:femme_mobile/ui/terminal/palette.dart';
import 'package:flutter/painting.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  group('charKey', () {
    test('Ctrl+C is Char(c) with CTRL', () {
      final k = charKey('C', mods: {Mods.ctrl});
      expect(k.code, {'Char': 'c'});
      expect(k.mods, {Mods.ctrl});
    });
    test('control characters map to named keys', () {
      expect(charKey('\n').code, KeyCode.enter);
      expect(charKey('\t').code, KeyCode.tab);
      expect(charKey('\x1b').code, KeyCode.escape);
      expect(charKey('A').code, {'Char': 'A'}, reason: 'no Ctrl: case kept');
    });
  });

  group('diffInput', () {
    test('a typed char', () {
      final d = diffInput('${inputSentinel}x');
      expect(d.keys.map((k) => k.code), [
        {'Char': 'x'},
      ]);
      expect(d.paste, isNull);
    });
    test('backspace', () {
      final d = diffInput(inputSentinel.substring(1));
      expect(d.keys.map((k) => k.code), [KeyCode.backspace]);
    });
    test('enter', () {
      expect(diffInput('$inputSentinel\n').keys.single.code, KeyCode.enter);
    });
    test('a replaced word (autocorrect): backspaces then the new text', () {
      final d = diffInput('ab');
      expect(d.keys.map((k) => k.code), [
        KeyCode.backspace,
        KeyCode.backspace,
        {'Char': 'a'},
        {'Char': 'b'},
      ]);
    });
    test('a long insertion is a paste', () {
      final text = 'x' * 40;
      final d = diffInput('$inputSentinel$text');
      expect(d.keys, isEmpty);
      expect(d.paste, text);
    });
    test('sticky mods apply to the first key only', () {
      final m = StickyMods()..tapCtrl();
      final d = diffInput('${inputSentinel}cd', mods: m.take);
      expect(d.keys.first.mods, {Mods.ctrl});
      expect(d.keys.last.mods, isEmpty);
    });
  });

  test('StickyMods: tap arms, second tap locks, third releases', () {
    final m = StickyMods()..tapCtrl();
    expect(m.take(), {Mods.ctrl});
    expect(m.take(), isEmpty);
    m
      ..tapCtrl()
      ..tapCtrl();
    expect(m.take(), {Mods.ctrl});
    expect(m.take(), {Mods.ctrl});
    m.tapCtrl();
    expect(m.take(), isEmpty);
  });

  test('extra keys send their codes', () {
    expect(ExtraKey.esc.code, KeyCode.escape);
    expect(ExtraKey.pgUp.code, KeyCode.pageUp);
    expect(ExtraKey.pipe.code, {'Char': '|'});
    expect(ExtraKey.ctrl.code, isNull);
  });

  group('layoutRow', () {
    test('ASCII stays together, columns counted', () {
      final p = layoutRow(
        const RowPatch(abs: 0, runs: [(0, 'ab'), (3, 'cd')]),
        80,
      );
      expect(
        [for (final x in p) (x.col, x.cols, x.style, x.text)],
        [(0, 2, 0, 'ab'), (2, 2, 3, 'cd')],
      );
    });
    test('a wide char takes two columns via wide_cols', () {
      final p = layoutRow(
        const RowPatch(abs: 0, runs: [(0, 'a漢b')], wideCols: [2]),
        80,
      );
      expect(
        [for (final x in p) (x.col, x.cols, x.text)],
        [(0, 1, 'a'), (1, 2, '漢'), (3, 1, 'b')],
      );
    });
    test('combining marks ride with their char', () {
      final p = layoutRow(const RowPatch(abs: 0, runs: [(0, 'éx')]), 80);
      expect(
        [for (final x in p) (x.col, x.cols, x.text)],
        [(0, 1, 'e\u0301'), (1, 1, 'x')],
      );
    });
    test('clipped at max cols; a wide char across the edge is dropped', () {
      final p = layoutRow(
        const RowPatch(abs: 0, runs: [(0, 'abc漢')], wideCols: [4]),
        4,
      );
      expect([for (final x in p) x.text], ['abc']);
    });
    test('cellAt finds the char under the cursor', () {
      const r = RowPatch(abs: 0, runs: [(0, 'a漢b')], wideCols: [2]);
      expect(cellAt(r, 0), 'a');
      expect(cellAt(r, 1), '漢');
      expect(cellAt(r, 3), 'b');
      expect(cellAt(r, 9), ' ');
    });
  });

  group('palette', () {
    test('ANSI, cube, grey ramp, RGB', () {
      const p = TermPalette.light;
      const def = Color(0xFF000001);
      expect(p.color(TermColor.defaultColor, def), def);
      expect(p.color(TermColor.fromJson({'Indexed': 1}), def), p.ansi[1]);
      expect(
        p.color(TermColor.fromJson({'Indexed': 196}), def),
        const Color(0xFFFF0000),
      );
      expect(
        p.color(TermColor.fromJson({'Indexed': 232}), def),
        const Color(0xFF080808),
      );
      expect(
        p.color(
          TermColor.fromJson({
            'Rgb': [1, 2, 3],
          }),
          def,
        ),
        const Color(0xFF010203),
      );
    });
    test('inverse swaps; a paper bg is none', () {
      const p = TermPalette.dark;
      final r = p.resolve(const Style(attrs: {Attrs.inverse}));
      expect(r.fg, p.bg);
      expect(r.bg, p.fg);
      expect(p.resolve(const Style()).bg, isNull);
    });
  });
}
