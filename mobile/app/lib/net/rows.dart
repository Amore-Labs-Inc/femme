import 'models.dart';
import 'wire.dart';

/// `state::Row`, one replicated row of `S2C::Changes`.
sealed class Row {
  const Row();
  int get rev;

  factory Row.fromJson(Object json) {
    final (tag, v) = tagOf(json);
    return switch (tag) {
      'Session' => SessionRow(Session.fromJson(v as Json)),
      'Pane' => PaneRow(Pane.fromJson(v as Json)),
      'Item' => ItemRow(Item.fromJson(v as Json)),
      'Issue' => IssueRow(Issue.fromJson(v as Json)),
      'Tombstone' => Tombstone.fromJson(v as Json),
      _ => UnknownRow(tag, v),
    };
  }

  Object toJson();
}

class SessionRow extends Row {
  const SessionRow(this.session);
  final Session session;
  @override
  int get rev => session.rev;
  @override
  Object toJson() => {'Session': session.toJson()};
}

class PaneRow extends Row {
  const PaneRow(this.pane);
  final Pane pane;
  @override
  int get rev => pane.rev;
  @override
  Object toJson() => {'Pane': pane.toJson()};
}

class ItemRow extends Row {
  const ItemRow(this.item);
  final Item item;
  @override
  int get rev => item.rev;
  @override
  Object toJson() => {'Item': item.toJson()};
}

class IssueRow extends Row {
  const IssueRow(this.issue);
  final Issue issue;
  @override
  int get rev => issue.rev;
  @override
  Object toJson() => {'Issue': issue.toJson()};
}

/// A delete. `id` is the entity's id as text: session ULID, `sid:no`, `sid/item_id`, issue number.
class Tombstone extends Row {
  const Tombstone(this.entity, this.id, this.rev);
  final Entity entity;
  final String id;
  @override
  final int rev;

  factory Tombstone.fromJson(Json j) => Tombstone(
    wireEnum(Entity.values, j['entity'], Entity.item),
    j['id'] as String,
    j['rev'] as int,
  );
  @override
  Object toJson() => {
    'Tombstone': {'entity': wireName(entity), 'id': id, 'rev': rev},
  };
}

/// A row kind this build does not know (a newer server); skipped by the store.
class UnknownRow extends Row {
  const UnknownRow(this.tag, this.value);
  final String tag;
  final Object? value;
  @override
  int get rev => 0;
  @override
  Object toJson() => {tag: value};
}
