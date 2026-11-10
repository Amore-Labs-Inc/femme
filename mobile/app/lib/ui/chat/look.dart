/// Small look helpers shared by the chat's pieces: the permission chip's colours, the monospace
/// style, the muted colour, and a tappable chip.
library;

import 'package:flutter/cupertino.dart';
import 'package:flutter/services.dart';

import '../../net/link.dart';
import '../../net/rpc.dart';
import '../../state/chat.dart';
import '../theme.dart';

/// Plan mode's purple: the desktop's `permission.rs` colour, the one hue the shared tokens lack.
const planPurple = Color(0xFF8A5CD6);

/// The permission mode's colour (PARITY §6): manual green, acceptEdits yellow, plan purple,
/// auto neon, bypassPermissions red, anything else grey.
Color permissionColor(String? p) => switch (p) {
  null || '' || 'default' => FemmeColors.work,
  'acceptEdits' => FemmeColors.pause,
  'plan' => planPurple,
  'auto' => FemmeColors.neon,
  'bypassPermissions' => FemmeColors.bad,
  _ => FemmeColors.done,
};

/// Cycles the session's permission mode with a haptic; returns the error text, if any.
Future<String?> cyclePermission(Requester link, String sid) async {
  HapticFeedback.selectionClick();
  try {
    await link.req(Req.cyclePermission(sid), resend: false);
    return null;
  } catch (e) {
    return errorText(e);
  }
}

const monoFamily = 'JetBrains Mono';
const monoFallback = ['Menlo', 'Roboto Mono', 'DejaVu Sans Mono', 'monospace'];

TextStyle mono(BuildContext context, {double size = 13, Color? color}) =>
    TextStyle(
      fontFamily: monoFamily,
      fontFamilyFallback: monoFallback,
      fontSize: size,
      height: 1.35,
      color: color ?? FemmeColors.inkC.resolveFrom(context),
    );

Color muted(BuildContext context) => FemmeColors.mutedC.resolveFrom(context);
Color ink(BuildContext context) => FemmeColors.inkC.resolveFrom(context);
Color panel(BuildContext context) => FemmeColors.panelC.resolveFrom(context);
Color paper(BuildContext context) => FemmeColors.paperC.resolveFrom(context);

/// A small bordered chip: a colour square and a label; tappable when [onTap] is set.
class Chip extends StatelessWidget {
  const Chip({super.key, required this.label, this.color, this.onTap});
  final String label;
  final Color? color;
  final VoidCallback? onTap;

  @override
  Widget build(BuildContext context) {
    final body = Container(
      constraints: const BoxConstraints(minHeight: 28),
      padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 4),
      decoration: BoxDecoration(
        border: Border.all(color: ink(context), width: 1.2),
        color: paper(context),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (color != null) ...[
            Container(
              width: 10,
              height: 10,
              decoration: BoxDecoration(
                color: color,
                border: Border.all(color: ink(context), width: 1),
              ),
            ),
            const SizedBox(width: 6),
          ],
          Flexible(
            child: Text(
              label,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: mono(context, size: 12),
            ),
          ),
        ],
      ),
    );
    if (onTap == null) return body;
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: onTap,
      child: Padding(
        padding: const EdgeInsets.symmetric(vertical: 8),
        child: body,
      ),
    );
  }
}

/// A one-line note for the user at the bottom of the screen (a toast), gone after 3 s.
void toast(BuildContext context, String text) {
  final overlay = Overlay.maybeOf(context);
  if (overlay == null) return;
  late final OverlayEntry e;
  e = OverlayEntry(
    builder: (ctx) => Positioned(
      left: 16,
      right: 16,
      bottom: MediaQuery.of(ctx).padding.bottom + 96,
      child: IgnorePointer(
        child: Container(
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 10),
          decoration: BoxDecoration(
            color: FemmeColors.inkC.resolveFrom(ctx),
            boxShadow: const [
              BoxShadow(color: FemmeColors.done, offset: Offset(3, 3)),
            ],
          ),
          child: Text(
            text,
            style: TextStyle(
              color: FemmeColors.paperC.resolveFrom(ctx),
              fontSize: 14,
              decoration: TextDecoration.none,
            ),
          ),
        ),
      ),
    ),
  );
  overlay.insert(e);
  Future.delayed(const Duration(seconds: 3), () {
    if (e.mounted) e.remove();
  });
}
