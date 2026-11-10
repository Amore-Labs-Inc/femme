import 'package:flutter/cupertino.dart';

/// The desktop's tokens (`mobile/DESIGN.md`, visual language).
abstract final class FemmeColors {
  static const paper = Color(0xFFFEFEFE);
  static const ink = Color(0xFF1E1E1E);
  static const darkPaper = Color(0xFF1E1E1E);
  static const darkInk = Color(0xFFECECEC);
  static const panel = Color(0xFFE5E5E5);
  static const darkPanel = Color(0xFF2A2A2A);
  static const muted = Color(0xFF5C5C5C);
  static const darkMuted = Color(0xFFA0A0A0);
  static const accent = Color(0xFFD45BB6);
  static const neon = Color(0xFFFF4FB4);

  static const work = Color(0xFF03AA5C);
  static const pause = Color(0xFFF2B33D);
  static const queue = Color(0xFFDEDEDE);
  static const done = Color(0xFF858585);
  static const bad = Color(0xFFD93036);
  static const pr = Color(0xFF09AEA1);

  static const inkC = CupertinoDynamicColor.withBrightness(
    color: ink,
    darkColor: darkInk,
  );
  static const paperC = CupertinoDynamicColor.withBrightness(
    color: paper,
    darkColor: darkPaper,
  );
  static const panelC = CupertinoDynamicColor.withBrightness(
    color: panel,
    darkColor: darkPanel,
  );
  static const mutedC = CupertinoDynamicColor.withBrightness(
    color: muted,
    darkColor: darkMuted,
  );
}

CupertinoThemeData femmeTheme(Brightness? brightness) => CupertinoThemeData(
  brightness: brightness,
  primaryColor: FemmeColors.accent,
  scaffoldBackgroundColor: FemmeColors.paperC,
  barBackgroundColor: FemmeColors.paperC,
  textTheme: const CupertinoTextThemeData(primaryColor: FemmeColors.accent),
);
