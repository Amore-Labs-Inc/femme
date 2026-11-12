/// The row of keys a phone keyboard lacks, above the keyboard: Esc, Tab, sticky Ctrl/Alt/Shift, arrows,
/// Home/End, PgUp/PgDn, `-` `/` `|` and paste. Keys like the system keyboard's: a rounded fill, no
/// border, a darker fill while pressed, 44 pt targets.
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
    return Container(
      height: 52,
      decoration: BoxDecoration(
        color: FemmeColors.panelC.resolveFrom(context),
        border: Border(
          top: BorderSide(
            color: CupertinoColors.separator.resolveFrom(context),
            width: 0,
          ),
        ),
      ),
      child: ListView(
        scrollDirection: Axis.horizontal,
        padding: const EdgeInsets.symmetric(horizontal: 6, vertical: 4),
        children: [
          for (final k in ExtraKey.values)
            _KeyButton(
              label: k.label,
              armed: k.mod != null && mods.armed(k.mod!),
              locked: k.mod != null && mods.locked(k.mod!),
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
    final Color fill;
    if (widget.armed) {
      fill = _down
          ? Color.lerp(FemmeColors.accent, ink, 0.2)!
          : FemmeColors.accent;
    } else {
      fill =
          (_down
                  ? CupertinoColors.systemFill
                  : CupertinoColors.tertiarySystemFill)
              .resolveFrom(context);
    }
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
          constraints: const BoxConstraints(minWidth: 44, minHeight: 44),
          padding: const EdgeInsets.symmetric(horizontal: 10),
          decoration: BoxDecoration(
            color: fill,
            borderRadius: const BorderRadius.all(Radius.circular(7)),
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
