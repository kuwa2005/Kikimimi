use crate::db::{self, Hit, Mark, Note, Question, Segment};

#[derive(Default)]
pub struct Minutes {
    pub title: String,
    pub summary_md: String,
    pub questions: Vec<Question>,
    pub segments: Vec<Segment>,
    pub pins: Vec<Hit>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub duration_sec: f64,
    pub participants: String,
    /// 参加者は書き起こしから自動で拾ったもの。
    pub participants_auto: bool,
    /// 会議中に AI が拾ったメモ（決定・宿題・数字・懸念・用語）。
    pub notes: Vec<Note>,
    /// 「ここ重要」の印。
    pub marks: Vec<Mark>,
}

pub fn note_label(kind: &str) -> &'static str {
    match kind {
        "decision" => "決定",
        "todo" => "宿題",
        "number" => "数字・日付",
        "concern" => "懸念",
        "term" => "用語",
        _ => "メモ",
    }
}

/// 議題の「・目安 10分・経過 12分」。どちらも無ければ空。
pub fn agenda_time(question: &Question, duration_sec: f64) -> String {
    let spent = db::spent_seconds(question, duration_sec);
    let mut out = String::new();
    if let Some(minutes) = question.minutes {
        out.push_str(&format!("・目安 {minutes}分"));
    }
    if spent >= 30.0 {
        out.push_str(&format!("・経過 {}分", (spent / 60.0).round().max(1.0) as i64));
    }
    out
}

pub fn question_label(question: &Question) -> String {
    match question.status.as_str() {
        "sufficient" => "完了".into(),
        "draft" => "途中".into(),
        "skipped" if question.skipped_by == "ai" && !question.skip_reason.is_empty() => {
            format!("今回は扱わない・AI の判断: {}", question.skip_reason)
        }
        "skipped" => "今回は扱わない".into(),
        _ => "未着手".into(),
    }
}

impl Minutes {
    /// その区間に「ここ重要」の印が付いているか。
    pub fn marked(&self, segment: &Segment) -> bool {
        self.marks.iter().any(|mark| db::marked_segment(mark, segment))
    }
}

/// 要約とエクスポートの材料。会議が無ければ None。
pub fn load_minutes(conn: &rusqlite::Connection, meeting_id: i64) -> anyhow::Result<Option<Minutes>> {
    let Some(meeting) = db::get_meeting(conn, meeting_id)? else {
        return Ok(None);
    };
    Ok(Some(Minutes {
        title: meeting.title,
        summary_md: meeting.summary_md,
        questions: db::list_questions(conn, meeting_id)?,
        segments: db::list_segments(conn, meeting_id)?,
        pins: db::pinned_hits(conn, meeting_id)?,
        started_at: meeting.started_at.or(Some(meeting.created_at)),
        ended_at: meeting.ended_at,
        duration_sec: meeting.duration_sec,
        participants: meeting.participants,
        participants_auto: meeting.participants_auto,
        notes: db::list_notes(conn, meeting_id)?,
        marks: db::list_marks(conn, meeting_id)?,
    }))
}

/// 保存した UTC の時刻を、サーバの地域時刻（TZ）で読みやすく直す。
pub fn local_time(stamp: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(stamp) {
        Ok(time) => time
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        Err(_) => stamp.to_string(),
    }
}

pub fn duration_text(sec: f64) -> String {
    let whole = sec.max(0.0).floor() as i64;
    let (h, m, s) = (whole / 3600, whole % 3600 / 60, whole % 60);
    if h > 0 {
        format!("{h}時間{m:02}分{s:02}秒")
    } else {
        format!("{m}分{s:02}秒")
    }
}

/// ダウンロード時のファイル名。`20261031_定例会議.md`
/// ファイル名に使えない文字を「_」にし、60 文字までにする。
pub fn safe_file_stem(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_control() || "\\/:*?\"<>|".contains(ch) {
                '_'
            } else {
                ch
            }
        })
        .take(60)
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn file_name(minutes: &Minutes) -> String {
    let date = minutes
        .started_at
        .as_deref()
        .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
        .map(|time| time.with_timezone(&chrono::Local).format("%Y%m%d").to_string())
        .unwrap_or_else(|| "gijiroku".into());
    let title = safe_file_stem(&minutes.title);
    let title = title.trim();
    if title.is_empty() {
        format!("{date}.md")
    } else {
        format!("{date}_{title}.md")
    }
}

fn timestamp(sec: f64) -> String {
    let whole = sec.max(0.0).floor() as i64;
    if whole >= 3600 {
        format!("{}:{:02}:{:02}", whole / 3600, whole % 3600 / 60, whole % 60)
    } else {
        format!("{:02}:{:02}", whole / 60, whole % 60)
    }
}

/// 出力する範囲。共有先に合わせて選ぶ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 見出し（日時・参加者）と要約だけ
    Summary,
    /// 要約と、聞くことリストの回答
    Qa,
    /// 記録一式（全文書き起こし・参照資料を含む）
    Full,
}

impl Scope {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "" | "full" => Some(Scope::Full),
            "summary" => Some(Scope::Summary),
            "qa" => Some(Scope::Qa),
            _ => None,
        }
    }
}

#[cfg(test)]
pub fn render(minutes: &Minutes) -> String {
    render_scope(minutes, Scope::Full)
}

pub fn render_scope(minutes: &Minutes, scope: Scope) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n", minutes.title));
    if let Some(started) = minutes.started_at.as_deref() {
        let ended = minutes
            .ended_at
            .as_deref()
            .map(local_time)
            .map(|time| format!(" 〜 {time}"))
            .unwrap_or_default();
        out.push_str(&format!("- 日時: {}{ended}\n", local_time(started)));
    }
    out.push_str(&format!(
        "- 録音時間: {}\n",
        duration_text(minutes.duration_sec)
    ));
    if !minutes.participants.trim().is_empty() {
        out.push_str(&format!("- 参加者: {}\n", minutes.participants.trim()));
    }
    out.push('\n');
    if minutes.summary_md.trim().is_empty() {
        out.push_str("（要約はまだありません）\n\n");
    } else {
        out.push_str(minutes.summary_md.trim());
        out.push_str("\n\n");
    }

    if scope == Scope::Summary {
        return out;
    }
    out.push_str("## アジェンダ\n\n");
    if minutes.questions.is_empty() {
        out.push_str("（なし）\n\n");
    }
    for question in &minutes.questions {
        let label = question_label(question);
        let added = if question.origin == "ai" { "（会議中に AI が追加）" } else { "" };
        let time = agenda_time(question, minutes.duration_sec);
        out.push_str(&format!(
            "### {}. {}{added} （{label}{time}）\n\n",
            question.position + 1,
            question.body
        ));
        if question.answer.trim().is_empty() {
            out.push_str("回答: （まだありません）\n\n");
        } else {
            out.push_str(&format!("回答: {}\n\n", question.answer.trim()));
        }
    }

    if !minutes.notes.is_empty() {
        out.push_str("## 会議中のメモ（AI が発言から拾ったもの）\n\n");
        for note in &minutes.notes {
            let at = note.start_sec.map(|sec| format!(" [{}]", timestamp(sec))).unwrap_or_default();
            let detail = if note.detail.is_empty() { String::new() } else { format!(" — {}", note.detail) };
            out.push_str(&format!("- {}: {}{detail}{at}\n", note_label(&note.kind), note.text));
        }
        out.push('\n');
    }

    if scope == Scope::Qa {
        return out;
    }
    out.push_str("## 全文書き起こし\n\n");
    if minutes.segments.is_empty() {
        out.push_str("（なし）\n\n");
    }
    for segment in &minutes.segments {
        out.push_str(&format!(
            "[{}] {}{}{}\n\n",
            timestamp(segment.start_sec),
            if minutes.marked(segment) { "★ " } else { "" },
            if segment.speaker.is_empty() { String::new() } else { format!("{}: ", segment.speaker) },
            segment.text
        ));
    }

    out.push_str("## 参照した資料（社内ナレッジ・顧客資料）\n\n");
    if minutes.pins.is_empty() {
        out.push_str("（ピン留めなし）\n");
    }
    for pin in &minutes.pins {
        let source = match pin.source.as_str() {
            "fess" => "FESS",
            "customer" => "顧客資料",
            _ => "ローカル",
        };
        out.push_str(&format!(
            "- [{source}] {} （{}）\n  検索語: {}\n  {}\n",
            pin.title, pin.locator, pin.query, pin.snippet
        ));
    }
    out
}

/// 自動要約ができなかったときの下書き。記録にある事実だけを並べ、推測で埋めない。
pub fn fallback_summary(minutes: &Minutes, error: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "> 自動要約に失敗したため、記録から下書きを組み立てました。{error}\n\n"
    ));
    out.push_str("## 会議の概要\n\n");
    out.push_str(&format!("- 会議: {}\n", minutes.title));
    if let Some(started) = minutes.started_at.as_deref() {
        out.push_str(&format!("- 日時: {}\n", local_time(started)));
    }
    out.push_str(&format!(
        "- 録音時間: {}\n",
        duration_text(minutes.duration_sec)
    ));
    if !minutes.participants.trim().is_empty() {
        out.push_str(&format!("- 参加者: {}\n", minutes.participants.trim()));
    }
    out.push_str("\n## 議論の背景・内容\n\n");
    if minutes.segments.is_empty() {
        out.push_str("- 書き起こしはありません\n");
    } else {
        out.push_str(&format!(
            "- 書き起こし {} 区間（下の「全文書き起こし」を参照）。主な発言の抜粋:\n",
            minutes.segments.len()
        ));
        // 会議全体から均等に抜き出す。先頭だけに偏らせない。
        let picks = 8.min(minutes.segments.len());
        let step = minutes.segments.len() as f64 / picks as f64;
        for index in 0..picks {
            let segment = &minutes.segments[(index as f64 * step) as usize];
            out.push_str(&format!(
                "  - [{}] {}\n",
                timestamp(segment.start_sec),
                segment.text
            ));
        }
    }
    out.push_str("\n## 決定事項\n\n");
    let decided: Vec<_> = minutes
        .questions
        .iter()
        .filter(|q| q.status == "sufficient" && !q.answer.trim().is_empty())
        .collect();
    if decided.is_empty() {
        out.push_str("- アジェンダ上で完了した議題はありません\n");
    } else {
        for question in decided {
            out.push_str(&format!(
                "- {}: {}\n",
                question.body,
                question.answer.trim()
            ));
        }
    }
    let drafts: Vec<_> = minutes
        .questions
        .iter()
        .filter(|q| q.status == "draft" && !q.answer.trim().is_empty())
        .collect();
    if !drafts.is_empty() {
        out.push_str("\n## 途中の議題\n\n");
        for question in drafts {
            out.push_str(&format!(
                "- {}: {}\n",
                question.body,
                question.answer.trim()
            ));
        }
    }
    out.push_str("\n## アクションアイテム\n\n");
    out.push_str("- 自動要約に失敗したため未抽出です。書き起こしから確認してください\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Minutes {
        Minutes {
            title: "月次".into(),
            summary_md: "## 会議の概要\n\n- テスト".into(),
            questions: vec![Question {
                id: 1,
                position: 0,
                body: "予算".into(),
                status: "draft".into(),
                answer: "未定".into(),
                locked: false,
                source: None,
                evidence: Vec::new(),
                origin: "human".into(),
                skipped_by: String::new(),
                skip_reason: String::new(),
                minutes: None,
                spent_sec: 0.0,
                active_since: None,
            }],
            segments: vec![Segment {
                id: 1,
                start_sec: 65.0,
                text: "来期の話".into(),
                ..Default::default()
            }],
            pins: vec![Hit {
                id: 1,
                source: "local".into(),
                title: "予算方針".into(),
                locator: "/docs/budget.md".into(),
                snippet: "上限は…".into(),
                query: "予算".into(),
                pinned: true,
            }],
            started_at: Some("2026-10-31T01:00:00Z".into()),
            ended_at: Some("2026-10-31T02:00:00Z".into()),
            duration_sec: 3725.0,
            participants: "田中、佐藤".into(),
            participants_auto: false,
            notes: Vec::new(),
            marks: Vec::new(),
        }
    }

    #[test]
    fn markdown_includes_pin() {
        let md = render(&sample());
        assert!(md.contains("# 月次"));
        assert!(md.contains("[01:05]"));
        assert!(md.contains("予算方針"));
        assert!(md.contains("## 参照した資料（社内ナレッジ・顧客資料）"));
        assert!(md.contains("- 日時: "));
        assert!(md.contains("- 録音時間: 1時間02分05秒"));
        assert!(md.contains("- 参加者: 田中、佐藤"));
    }

    #[test]
    fn scopes_cut_the_record() {
        let summary = render_scope(&sample(), Scope::Summary);
        assert!(summary.contains("## 会議の概要") && summary.contains("- 参加者"));
        assert!(!summary.contains("## アジェンダ") && !summary.contains("来期の話") && !summary.contains("/docs/budget.md"));
        let qa = render_scope(&sample(), Scope::Qa);
        assert!(qa.contains("回答: 未定") && !qa.contains("来期の話"));
        assert!(render_scope(&sample(), Scope::Full).contains("/docs/budget.md"));
        assert_eq!(Scope::parse("bogus"), None);
    }

    #[test]
    fn fallback_does_not_list_answers_as_action_items() {
        let md = fallback_summary(&sample(), "429");
        let actions = md.split("## アクションアイテム").nth(1).unwrap();
        assert!(!actions.contains("未定"));
        assert!(md.contains("## 途中の議題"));
        assert!(md.contains("## 会議の概要\n\n"));
    }

    #[test]
    fn file_name_has_date_and_safe_title() {
        let mut minutes = sample();
        minutes.title = "定例/会議:10月".into();
        let name = file_name(&minutes);
        assert!(name.ends_with("_定例_会議_10月.md"), "{name}");
        assert!(name.starts_with("2026"));
    }
}
