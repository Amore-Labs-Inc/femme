/// The row of keys a phone keyboard lacks, above the keyboard: Esc, Tab, sticky Ctrl/Alt, arrows,
/// Home/End, PgUp/PgDn, `-` `/` `|` and paste. Flat, ink-bordered, 44 px targets.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../theme.dart';
import 'keys.dart';

class ExtraKeysRow extends StatelessWidget {
  const ExtraKeysRow({super.key, required this.mods, required this.onKey});

  final StickyMods mods;
  final void Function(ExtraKey key) onKey;

  @override
  Widget build(BuildContext context) {
    final ink = FemmeColors.inkC.resolveFrom(context);
    return Container(
      height: 48,
      decoration: BoxDecoration(
        color: FemmeColors.panelC.resolveFrom(context),
        border: Border(top: BorderSide(color: ink, width: 1.2)),
      ),
      child: ListView(
        scrollDirection: Axis.horizontal,
        padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 4),
        children: [
          for (final k in ExtraKey.values)
            _KeyButton(
              label: k.label,
              armed:
                  (k == ExtraKey.ctrl && mods.ctrl) ||
                  (k == ExtraKey.alt && mods.alt),
              locked:
                  (k == ExtraKey.ctrl && mods.ctrlLocked) ||
                  (k == ExtraKey.alt && mods.altLocked),
              onTap: () {
                HapticFeedback.lightImpact();
                onKey(k);
              },
            ),
        ],
      ),
    );
  }
}

class _KeyButton extends StatefulWidget {
  const _KeyButton({
    required this.label,
    required this.onTap,
    this.armed = false,
    this.locked = false,
  });
  final String label;
  final VoidCallback onTap;
  final bool armed;
  final bool locked;

  @override
  State<_KeyButton> createState() => _KeyButtonState();
}

class _KeyButtonState extends State<_KeyButton> {
  bool _down = false;

  @override
  Widget build(BuildContext context) {
    final ink = FemmeColors.inkC.resolveFrom(context);
    final paper = FemmeColors.paperC.resolveFrom(context);
    final fill = widget.armed ? FemmeColors.accent : paper;
    final fg = widget.armed ? FemmeColors.paper : ink;
    return Padding(
      padding: const EdgeInsets.symmetric(horizontal: 3),
      child: GestureDetector(
        onTapDown: (_) => setState(() => _down = true),
        onTapUp: (_) => setState(() => _down = false),
        onTapCancel: () => setState(() => _down = false),
        onTap: widget.onTap,
        child: AnimatedContainer(
          duration: const Duration(milliseconds: 90),
          constraints: const BoxConstraints(minWidth: 44),
          padding: const EdgeInsets.symmetric(horizontal: 10),
          transform: Matrix4.translationValues(_down ? 2 : 0, _down ? 2 : 0, 0),
          decoration: BoxDecoration(
            color: _down ? FemmeColors.neon : fill,
            border: Border.all(color: ink, width: 1.2),
            boxShadow: _down
                ? null
                : [BoxShadow(color: ink, offset: const Offset(2, 2))],
          ),
          alignment: Alignment.center,
          child: Text(
            widget.label,
            style: TextStyle(
              color: fg,
              fontSize: 15,
              fontWeight: FontWeight.w500,
              decoration: widget.locked ? TextDecoration.underline : null,
              decorationThickness: 2,
            ),
          ),
        ),
      ),
    );
  }
}
