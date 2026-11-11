/// Small look helpers shared by the chat's pieces: the permission chip's colours, the monospace
/// style, the muted colour, the iOS radii and fills, a capsule chip and the toast.
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

/// The phone's iOS shapes (the desktop's square, hard-shadowed look is not used here).
const bubbleRadius = 18.0;
const cardRadius = 12.0;
const codeRadius = 10.0;

/// The composer's capsule text field, as iOS Messages.
const composerRadius = 20.0;

/// A soft iOS drop shadow for floating panels (popups, toasts).
const softShadow = [
  BoxShadow(color: Color(0x1F000000), blurRadius: 12, offset: Offset(0, 4)),
];

/// The hairline width: one physical pixel.
double hairline(BuildContext context) =>
    1 / MediaQuery.devicePixelRatioOf(context);

Color fill(BuildContext context) =>
    CupertinoColors.tertiarySystemFill.resolveFrom(context);
Color separator(BuildContext context) =>
    CupertinoColors.separator.resolveFrom(context);

/// A capsule chip: an optional colour dot (which also tints the capsule) and a label;
/// tappable when [onTap] is set.
class Chip extends StatelessWidget {
  const Chip({super.key, required this.label, this.color, this.onTap});
  final String label;
  final Color? color;
  final VoidCallback? onTap;

  @override
  Widget build(BuildContext context) {
    final c = color;
    final body = Container(
      constraints: const BoxConstraints(minHeight: 28),
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 4),
      decoration: BoxDecoration(
        borderRadius: BorderRadius.circular(999),
        color: c == null ? fill(context) : c.withValues(alpha: 0.18),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (c != null) ...[
            Container(
              width: 8,
              height: 8,
              decoration: BoxDecoration(color: c, shape: BoxShape.circle),
            ),
            const SizedBox(width: 6),
          ],
          Flexible(
            child: Text(
              label,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: mono(context, size: 13),
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
          key: const ValueKey('toast'),
          padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
          decoration: BoxDecoration(
            color: FemmeColors.inkC.resolveFrom(ctx),
            borderRadius: BorderRadius.circular(cardRadius),
            boxShadow: softShadow,
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
