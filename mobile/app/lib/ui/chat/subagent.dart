/// A subagent's own transcript: the items whose `parent` is its `Agent` call id.
library;

import 'package:flutter/cupertino.dart';

import '../../state/chat.dart';
import 'media.dart';
import 'transcript.dart';

class SubagentScreen extends StatelessWidget {
  const SubagentScreen({
    super.key,
    required this.sid,
    required this.callId,
    required this.title,
    required this.controller,
  });
  final String sid;
  final String callId;
  final String title;

  /// The chat's controller (the chat screen below owns it).
  final ChatController controller;

  @override
  Widget build(BuildContext context) => CupertinoPageScaffold(
    navigationBar: CupertinoNavigationBar(
      middle: Text(title, maxLines: 1, overflow: TextOverflow.ellipsis),
      previousPageTitle: 'main',
    ),
    child: SafeArea(
      child: ChatScope(
        controller: controller,
        child: TranscriptView(
          sid: sid,
          parent: callId,
          empty: 'The subagent has not said anything yet.',
        ),
      ),
    ),
  );
}
