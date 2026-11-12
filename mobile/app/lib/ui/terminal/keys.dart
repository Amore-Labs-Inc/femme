/// Keys from the phone to the terminal: the soft keyboard's text turned into key presses, the
/// extra-keys row, and the sticky Ctrl/Alt/Shift modifiers. The holder encodes keys into bytes with the
/// live terminal modes, so here a key is only its code and mods.
library;

import 'package:flutter/services.dart';

import '../../net/term_models.dart';

/// One key press: a `KeyCode` value and its modifier names.
typedef TermKey = ({Object code, Set<String> mods});

/// Ctrl, Alt and Shift as the extra-keys row holds them: a tap arms one for the next key or
/// touch, a second tap locks it until tapped again.
class StickyMods {
  final _armed = <String>{}, _locked = <String>{};

  bool get ctrl => armed(Mods.ctrl);
  bool get alt => armed(Mods.alt);
  bool get shift => armed(Mods.shift);
  bool get ctrlLocked => locked(Mods.ctrl);
  bool get altLocked => locked(Mods.alt);
  bool get shiftLocked => locked(Mods.shift);

  bool armed(String mod) => _armed.contains(mod);
  bool locked(String mod) => _locked.contains(mod);

  /// The armed mods in `Mods`' bit order (SHIFT, ALT, CTRL), as bitflags writes them.
  Set<String> get mods => {
    for (final m in const [Mods.shift, Mods.alt, Mods.ctrl])
      if (_armed.contains(m)) m,
  };

  /// Off → armed → locked → off.
  void tap(String mod) {
    if (_locked.remove(mod)) {
      _armed.remove(mod);
    } else if (!_armed.add(mod)) {
      _locked.add(mod);
    }
  }

  void tapCtrl() => tap(Mods.ctrl);
  void tapAlt() => tap(Mods.alt);
  void tapShift() => tap(Mods.shift);

  /// The mods for one key press or touch; armed (not locked) ones are used up.
  Set<String> take() {
    final m = mods;
    _armed.retainAll(_locked);
    return m;
  }
}

/// A typed character as a key: `\n` is Enter, `\t` Tab, the rest `Char`. With Ctrl a letter is
/// sent lower-case (`Ctrl+C` → `Char('c')` + `CTRL`) and with Shift upper-case (`Char('A')` +
/// `SHIFT`), as a keyboard sends it.
TermKey charKey(String ch, {Set<String> mods = const {}}) {
  final m = {...mods};
  switch (ch) {
    case '\n' || '\r':
      return (code: KeyCode.enter, mods: m);
    case '\t':
      return (code: KeyCode.tab, mods: m);
    case '\x7f' || '\b':
      return (code: KeyCode.backspace, mods: m);
    case '\x1b':
      return (code: KeyCode.escape, mods: m);
  }
  var c = ch;
  if (m.contains(Mods.ctrl) && c.length == 1) c = c.toLowerCase();
  if (m.contains(Mods.shift) && c.length == 1) c = c.toUpperCase();
  return (code: KeyCode.char(c), mods: m);
}

/// The text the hidden field is reset to after every edit. Two invisible characters, so a
/// backspace always has something to delete and the IME never sees an empty field.
const inputSentinel = '​​';

/// What changed in the hidden field, from [inputSentinel] to [now], as keys: characters removed
/// are Backspaces, characters added are typed. Returns the keys and, for a long insertion (a
/// paste, dictation), the text to send as one paste instead.
({List<TermKey> keys, String? paste}) diffInput(
  String now, {
  Set<String> Function()? mods,
  int pasteOver = 32,
}) {
  const s = inputSentinel;
  // Common prefix with the sentinel; whatever of it is missing was backspaced.
  var p = 0;
  while (p < s.length && p < now.length && s[p] == now[p]) {
    p++;
  }
  // Characters of the sentinel after the prefix that survive at the end of `now`.
  var tail = 0;
  while (tail < s.length - p &&
      tail < now.length - p &&
      s[s.length - 1 - tail] == now[now.length - 1 - tail]) {
    tail++;
  }
  final removed = s.length - p - tail;
  final added = now.substring(p, now.length - tail).replaceAll('​', '');
  final keys = <TermKey>[
    for (var i = 0; i < removed; i++)
      (code: KeyCode.backspace, mods: mods?.call() ?? const {}),
  ];
  if (added.runes.length > pasteOver) return (keys: keys, paste: added);
  for (final r in added.runes) {
    keys.add(charKey(String.fromCharCode(r), mods: mods?.call() ?? const {}));
  }
  return (keys: keys, paste: null);
}

/// A hardware key (a Bluetooth keyboard, the simulator) as a terminal key, or null to let the
/// text field have it.
TermKey? hardwareKey(KeyEvent e, Set<String> held) {
  if (e is KeyUpEvent) return null;
  final k = e.logicalKey;
  final named = <LogicalKeyboardKey, Object>{
    LogicalKeyboardKey.arrowUp: KeyCode.up,
    LogicalKeyboardKey.arrowDown: KeyCode.down,
    LogicalKeyboardKey.arrowLeft: KeyCode.left,
    LogicalKeyboardKey.arrowRight: KeyCode.right,
    LogicalKeyboardKey.home: KeyCode.home,
    LogicalKeyboardKey.end: KeyCode.end,
    LogicalKeyboardKey.pageUp: KeyCode.pageUp,
    LogicalKeyboardKey.pageDown: KeyCode.pageDown,
    LogicalKeyboardKey.escape: KeyCode.escape,
    LogicalKeyboardKey.tab: KeyCode.tab,
    LogicalKeyboardKey.insert: KeyCode.insert,
    LogicalKeyboardKey.delete: KeyCode.delete,
  };
  final hw = HardwareKeyboard.instance;
  final mods = {
    ...held,
    if (hw.isControlPressed) Mods.ctrl,
    if (hw.isAltPressed) Mods.alt,
    if (hw.isShiftPressed) Mods.shift,
    if (hw.isMetaPressed) Mods.super_,
  };
  final code = named[k];
  if (code != null) return (code: code, mods: mods);
  for (var n = 1; n <= 12; n++) {
    if (k.keyLabel == 'F$n') return (code: KeyCode.f(n), mods: mods);
  }
  // Ctrl/Alt + a character: the text field would drop the modifier.
  final ch = e.character;
  if ((hw.isControlPressed || hw.isAltPressed) && ch != null && ch.isNotEmpty) {
    final label = k.keyLabel.length == 1 ? k.keyLabel.toLowerCase() : ch;
    return charKey(label, mods: mods..remove(Mods.shift));
  }
  return null;
}

/// One button of the extra-keys row.
enum ExtraKey {
  esc('Esc'),
  tab('Tab'),
  ctrl('Ctrl'),
  alt('Alt'),
  shift('Shift'),
  left('←'),
  up('↑'),
  down('↓'),
  right('→'),
  home('Home'),
  end('End'),
  pgUp('PgUp'),
  pgDn('PgDn'),
  dash('-'),
  slash('/'),
  pipe('|'),
  paste('Paste');

  const ExtraKey(this.label);
  final String label;

  /// The sticky modifier it toggles (`Mods` name), or null.
  String? get mod => switch (this) {
    ctrl => Mods.ctrl,
    alt => Mods.alt,
    shift => Mods.shift,
    _ => null,
  };

  /// The key code it sends, or null for the modifiers and paste.
  Object? get code => switch (this) {
    esc => KeyCode.escape,
    tab => KeyCode.tab,
    left => KeyCode.left,
    up => KeyCode.up,
    down => KeyCode.down,
    right => KeyCode.right,
    home => KeyCode.home,
    end => KeyCode.end,
    pgUp => KeyCode.pageUp,
    pgDn => KeyCode.pageDown,
    dash => KeyCode.char('-'),
    slash => KeyCode.char('/'),
    pipe => KeyCode.char('|'),
    ctrl || alt || shift || paste => null,
  };
}
