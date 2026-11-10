import 'package:flutter/cupertino.dart';

/// A session's terminal pane. Stub: the terminal screen replaces this file.
class TerminalScreen extends StatelessWidget {
  const TerminalScreen({super.key, required this.sid, this.pane});

  final String sid;

  /// The pane number to show (0 = the agent's pane of a TUI session); null = the first pane.
  final int? pane;

  @override
  Widget build(BuildContext context) => const CupertinoPageScaffold(
    navigationBar: CupertinoNavigationBar(middle: Text('Terminal')),
    child: Center(child: Text('terminal: coming')),
  );
}
