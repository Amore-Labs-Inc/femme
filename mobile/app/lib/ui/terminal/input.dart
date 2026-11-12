/// The hidden text field that brings up the soft keyboard and turns what is typed into keys.
/// It always holds [inputSentinel]; every edit is diffed against it and the field reset.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../theme.dart';
import 'keys.dart';

class TermInput extends StatefulWidget {
  const TermInput({
    super.key,
    required this.focus,
    required this.mods,
    required this.onKey,
    required this.onPaste,
  });

  final FocusNode focus;
  final StickyMods mods;
  final void Function(TermKey key) onKey;
  final void Function(String text) onPaste;

  @override
  State<TermInput> createState() => _TermInputState();
}

class _TermInputState extends State<TermInput> {
  final _ctl = TextEditingController(text: inputSentinel);

  @override
  void initState() {
    super.initState();
    _reset();
  }

  @override
  void dispose() {
    _ctl.dispose();
    super.dispose();
  }

  void _reset() {
    _ctl.value = const TextEditingValue(
      text: inputSentinel,
      selection: TextSelection.collapsed(offset: inputSentinel.length),
    );
  }

  void _onChanged(String now) {
    // An IME still composing (kana, pinyin) owns the field until it commits.
    final composing = _ctl.value.composing;
    if (composing.isValid && !composing.isCollapsed) return;
    if (now == inputSentinel) return;
    final d = diffInput(now, mods: widget.mods.take);
    for (final k in d.keys) {
      widget.onKey(k);
    }
    final p = d.paste;
    if (p != null) widget.onPaste(p);
    if (d.keys.isNotEmpty || p != null) HapticFeedback.lightImpact();
    _reset();
  }

  KeyEventResult _onHardware(FocusNode node, KeyEvent e) {
    final k = hardwareKey(e, widget.mods.mods);
    if (k == null) return KeyEventResult.ignored;
    widget.mods.take();
    widget.onKey(k);
    return KeyEventResult.handled;
  }

  @override
  Widget build(BuildContext context) => Focus(
    onKeyEvent: _onHardware,
    skipTraversal: true,
    child: SizedBox(
      width: 1,
      height: 1,
      child: Opacity(
        opacity: 0,
        child: EditableText(
          controller: _ctl,
          focusNode: widget.focus,
          style: const TextStyle(fontSize: 1),
          cursorColor: FemmeColors.accent,
          backgroundCursorColor: FemmeColors.accent,
          maxLines: null,
          keyboardType: TextInputType.multiline,
          textInputAction: TextInputAction.newline,
          autocorrect: false,
          enableSuggestions: false,
          smartDashesType: SmartDashesType.disabled,
          smartQuotesType: SmartQuotesType.disabled,
          enableIMEPersonalizedLearning: false,
          textCapitalization: TextCapitalization.none,
          onChanged: _onChanged,
        ),
      ),
    ),
  );
}
