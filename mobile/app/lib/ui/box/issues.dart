import 'package:flutter/cupertino.dart';

import '../../net/models.dart';
import '../../net/rpc.dart';
import '../femme.dart';
import '../theme.dart';
import '../widgets.dart';
import 'prs.dart';

Color issueColor(IssueStatus s) => switch (s) {
  IssueStatus.open => FemmeColors.queue,
  IssueStatus.working => FemmeColors.work,
  IssueStatus.waiting => FemmeColors.pause,
  IssueStatus.paused => FemmeColors.done,
  IssueStatus.pr => FemmeColors.pr,
  IssueStatus.done => FemmeColors.accent,
  IssueStatus.failed => FemmeColors.bad,
};

/// Issues by status (in [IssueStatus] order), newest change first within one.
List<Issue> sortedIssues(Iterable<Issue> all) => all.toList()
  ..sort((a, b) {
    final c = a.status.index.compareTo(b.status.index);
    return c != 0 ? c : b.updated.compareTo(a.updated);
  });

/// The issue queue (`Row::Issue` from the global scope): add, change status, delete.
class IssuesSection extends StatelessWidget {
  const IssuesSection({super.key});

  Future<void> _create(BuildContext context) async {
    final f = Femme.of(context);
    final repos = {for (final i in f.store.issues.values) i.repo}.toList()
      ..sort();
    final made = await showCupertinoSheet<(String, String, String)>(
      context: context,
      scrollableBuilder: (_, _) => _NewIssueSheet(repos: repos),
    );
    if (made == null || !context.mounted) return;
    try {
      await f.link.req(Req.issueCreate(made.$1, made.$2, made.$3));
      actionHaptic();
    } catch (e) {
      if (context.mounted) await showProblem(context, 'Issue not added', e);
    }
  }

  Future<void> _actions(BuildContext context, Issue i) async {
    final link = Femme.of(context).link;
    final pick = await showCupertinoModalPopup<Object>(
      context: context,
      builder: (c) => CupertinoActionSheet(
        title: Text('#${i.id} ${i.title}'),
        message: i.body.isEmpty
            ? null
            : Text(i.body, maxLines: 8, overflow: TextOverflow.ellipsis),
        actions: [
          for (final s in IssueStatus.values)
            if (s != i.status)
              CupertinoActionSheetAction(
                onPressed: () => Navigator.pop(c, s),
                child: Text('Mark ${s.name}'),
              ),
          if (i.session != null)
            CupertinoActionSheetAction(
              onPressed: () => Navigator.pop(c, 'chat'),
              child: const Text('Open its chat'),
            ),
          CupertinoActionSheetAction(
            isDestructiveAction: true,
            onPressed: () => Navigator.pop(c, 'delete'),
            child: const Text('Delete'),
          ),
        ],
        cancelButton: CupertinoActionSheetAction(
          onPressed: () => Navigator.pop(c),
          child: const Text('Cancel'),
        ),
      ),
    );
    if (pick == null || !context.mounted) return;
    try {
      if (pick is IssueStatus) {
        await link.req(Req.issuePatch(i.id, i.rev, [IssueField.status(pick)]));
        actionHaptic();
      } else if (pick == 'chat') {
        await pushChat(context, i.session!);
      } else if (pick == 'delete') {
        final ok = await confirmAction(
          context,
          title: 'Delete #${i.id}?',
          message: i.title,
          action: 'Delete',
        );
        if (!ok) return;
        await link.req(Req.issueDelete(i.id));
        actionHaptic();
      }
    } catch (e) {
      if (context.mounted) await showProblem(context, 'Not changed', e);
    }
  }

  @override
  Widget build(BuildContext context) {
    final f = Femme.of(context);
    final muted = FemmeColors.mutedC.resolveFrom(context);
    final issues = sortedIssues(f.store.issues.values);
    return CupertinoListSection.insetGrouped(
      header: Text('Issues · ${issues.length}'),
      children: [
        for (final i in issues)
          CupertinoListTile(
            title: Text(i.title, maxLines: 1, overflow: TextOverflow.ellipsis),
            subtitle: Text(
              [
                i.repo,
                ?i.note,
                '${ago(DateTime.fromMillisecondsSinceEpoch(i.updated))} ago',
              ].join(' · '),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
            ),
            additionalInfo: StatusChip(
              i.status.name,
              issueColor(i.status),
              faint: i.status == IssueStatus.open,
            ),
            onTap: () {
              tapHaptic();
              _actions(context, i);
            },
          ),
        if (issues.isEmpty)
          CupertinoListTile(
            title: Text('the queue is empty', style: TextStyle(color: muted)),
          ),
        CupertinoListTile(
          leading: const Icon(CupertinoIcons.add_circled),
          title: const Text('New issue'),
          onTap: () => _create(context),
        ),
      ],
    );
  }
}

class _NewIssueSheet extends StatefulWidget {
  const _NewIssueSheet({required this.repos});
  final List<String> repos;

  @override
  State<_NewIssueSheet> createState() => _NewIssueSheetState();
}

class _NewIssueSheetState extends State<_NewIssueSheet> {
  late final _repo = TextEditingController(
    text: widget.repos.isEmpty ? '' : widget.repos.first,
  );
  final _title = TextEditingController();
  final _body = TextEditingController();

  @override
  void dispose() {
    _repo.dispose();
    _title.dispose();
    _body.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    final ok = _repo.text.trim().isNotEmpty && _title.text.trim().isNotEmpty;
    Widget field(TextEditingController c, String hint, {int lines = 1}) =>
        Padding(
          padding: const EdgeInsets.all(10),
          child: CupertinoTextField(
            controller: c,
            placeholder: hint,
            minLines: lines,
            maxLines: lines == 1 ? 1 : 12,
            autocorrect: lines > 1,
            onChanged: (_) => setState(() {}),
          ),
        );
    return SheetPage(
      title: 'New issue',
      doneLabel: 'Add',
      onDone: ok
          ? () => Navigator.of(context)
                .pop((_repo.text.trim(), _title.text.trim(), _body.text.trim()))
          : null,
      child: ListView(
        children: [
          CupertinoListSection.insetGrouped(
            children: [
              field(_repo, 'owner/repo'),
              if (widget.repos.length > 1)
                SizedBox(
                  height: 40,
                  child: ListView(
                    scrollDirection: Axis.horizontal,
                    padding: const EdgeInsets.symmetric(horizontal: 6),
                    children: [
                      for (final r in widget.repos)
                        CupertinoButton(
                          sizeStyle: CupertinoButtonSize.small,
                          onPressed: () => setState(() => _repo.text = r),
                          child: Text(r),
                        ),
                    ],
                  ),
                ),
              field(_title, 'title'),
              field(_body, 'details', lines: 5),
            ],
          ),
        ],
      ),
    );
  }
}
