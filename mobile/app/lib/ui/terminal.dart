import 'package:flutter/cupertino.dart';

import 'terminal/screen.dart';

/// A session's terminal panes: the grid, the keyboard and the extra-keys row.
class TerminalScreen extends StatelessWidget {
  const TerminalScreen({super.key, required this.sid, this.pane});

  final String sid;

  /// The pane number to show (0 = the agent's pane of a TUI session); null = the first pane.
  final int? pane;

  @override
  Widget build(BuildContext context) =>
      TerminalScreenBody(sid: sid, pane: pane);
}
