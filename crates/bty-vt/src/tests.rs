use super::*;
use scan::CONTINUATION_MAX_BYTES;

#[test]
fn continuation_restores_split_sequences_and_long_strings_resume_at_ground() {
    for prefix in [b"\x1b_Gm=1;AAAA".as_slice(), b"\x1b]2;title", b"\x1bP+q544", b"\x1b[31", b"\xf0\x9f"] {
        let mut screen = Terminal::new(80, 24, 1 << 20);
        screen.write(prefix);
        assert_eq!(screen.continuation().unwrap(), prefix);
    }
    let mut screen = Terminal::new(80, 24, 1 << 20);
    screen.write(b"\x1b_Gm=1;");
    screen.write(&vec![b'A'; CONTINUATION_MAX_BYTES]);
    assert!(screen.continuation().is_none());
    assert_eq!(screen.write_until_ground(b"AAAA"), None);
    assert_eq!(screen.write_until_ground(b"AA\x1b\\\x1b_Gm=1;BBBB"), Some(4));
    assert_eq!(screen.continuation(), Some(Vec::new()));
}

#[test]
fn display_changes_report_to_subscribers_and_queries_use_the_latest_facts() {
    let mut t = Terminal::new(80, 24, 0);
    t.write(b"\x1b[?2031h");
    t.take_responses(); // Enabling reporting may itself report the current scheme.
    t.set_display(Some([1, 2, 3]), Some([255; 3]), Some([4, 5, 6]), Some((9, 18)));
    assert_eq!(t.take_responses(), b"\x1b[?997;2n", "dark to light");
    t.set_display(None, Some([0; 3]), None, None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "light to dark");

    // Mode 2031 also tells subscribers to re-query colors within the same scheme.
    t.set_display(Some([7, 8, 9]), None, None, None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "foreground changed");
    t.set_display(None, None, Some([10, 11, 12]), None);
    assert_eq!(t.take_responses(), b"\x1b[?997;1n", "cursor changed");
    t.set_display(Some([7, 8, 9]), Some([0; 3]), Some([10, 11, 12]), Some((10, 20)));
    t.set_display(None, None, None, None);
    assert!(t.take_responses().is_empty(), "repeated colors and cell size are silent");

    t.write(b"\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07\x1b[?996n\x1b[16t");
    assert_eq!(
        t.take_responses(),
        b"\x1b]10;rgb:0707/0808/0909\x07\x1b]11;rgb:0000/0000/0000\x07\x1b]12;rgb:0a0a/0b0b/0c0c\x07\x1b[?997;1n\x1b[6;20;10t"
    );
}

#[test]
fn display_changes_do_not_report_without_a_subscription() {
    let mut t = Terminal::new(80, 24, 0);
    t.set_display(None, Some([255; 3]), None, None);
    assert!(t.take_responses().is_empty());
    t.write(b"\x1b[?2031h\x1b[?2031l");
    t.take_responses();
    t.set_display(None, Some([0; 3]), None, None);
    assert!(t.take_responses().is_empty(), "disabled subscription stays silent");
}

#[test]
fn a_repaint_into_a_dirty_viewer_turns_off_modes_the_screen_no_longer_has() {
    let mut viewer = Terminal::new(40, 10, 0);
    viewer.write(b"\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1004h");
    let screen = Terminal::new(40, 10, 0);
    viewer.write(&screen.paint(0));
    for mode in [1000, 1006, 2004, 1004] {
        assert!(!viewer.dec_mode(mode), "mode {mode} left on");
    }
    let mut mouse = Terminal::new(40, 10, 0);
    mouse.write(b"\x1b[?1000h");
    viewer.write(&mouse.paint(0));
    assert!(viewer.dec_mode(1000), "and the screen's own modes come back");
}

#[test]
fn parses_reads_and_round_trips_a_snapshot() {
    let mut t = Terminal::new(20, 3, 1 << 20);
    for i in 0..10 {
        t.write(format!("line {i}\r\n").as_bytes());
    }
    let text = t.plain_text();
    assert!(text.contains("line 0") && text.contains("line 9"), "{text:?}");

    // Snapshot taken in the middle of an escape sequence finishes it after restore.
    t.write(b"\x1b[3");
    let snap = t.snapshot().unwrap();
    let mut back = Terminal::restore(&snap, 1 << 20).unwrap();
    back.write(b"1mred\x1b[0m");
    assert_eq!(back.size(), (20, 3));
    let text = back.plain_text();
    assert!(text.contains("line 0") && text.contains("red"), "{text:?}");
    assert!(!text.contains("1mred"), "{text:?}");

    t.write(b"m\x1b[?1049h");
    assert!(t.alt_screen());
    assert!(Terminal::restore(b"garbage", 1 << 20).is_none());
}

#[test]
fn a_painted_terminal_looks_the_same_on_a_fresh_one() {
    let mut t = Terminal::new(30, 4, 1 << 20);
    for i in 0..8 {
        t.write(format!("\x1b[31mline {i}\x1b[0m\r\n").as_bytes());
    }
    t.write(b"\x1b]2;my title\x07\x1b[?2026h\x1b[?1004hprompt> ");
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&t.paint(3));
    let screen = |t: &Terminal| t.plain_text().lines().rev().take(4).map(String::from).collect::<Vec<_>>();
    assert_eq!(screen(&fresh), screen(&t));
    assert!(fresh.plain_text().contains("line 2"), "{:?}", fresh.plain_text());
    assert!(!fresh.plain_text().contains("line 1\n"), "history beyond 3 lines");
    assert_eq!(fresh.title(), "my title");
    assert!(fresh.dec_mode(1004));
    assert!(!fresh.dec_mode(2026), "synchronized output must not be replayed");

    let mut alt = Terminal::new(30, 4, 1 << 20);
    alt.write(b"main\r\n\x1b[?1049h\x1b[2;3Hfull screen");
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&alt.paint(10));
    assert!(fresh.alt_screen());
    assert!(fresh.plain_text().contains("full screen"));
}

#[test]
fn attaching_on_alternate_screen_preserves_primary_when_the_program_exits() {
    for mode in [47, 1047, 1049] {
        for history in [0, 3] {
            let mut source = Terminal::new(30, 4, 1 << 20);
            for i in 0..8 {
                source.write(format!("line {i}\r\n").as_bytes());
            }
            source.write(format!("\x1b[31mprompt> \x1b[?{mode}h\x1b[2;3HALT").as_bytes());
            source.write(b"\x1b[");
            let before = source.snapshot().unwrap();
            let mut viewer = Terminal::new(30, 4, 1 << 20);
            viewer.write(b"stale primary\x1b[?1049hSTALE ALT");
            viewer.write(&source.paint(history));
            assert_eq!(source.snapshot().unwrap(), before, "painting must not change the live terminal");
            assert_eq!(viewer.screen_rows(), source.screen_rows());
            viewer.write(&source.continuation().unwrap());
            source.write(b"0m");
            viewer.write(b"0m");
            let exit = format!("\x1b[?{mode}lafter\r\n");
            source.write(exit.as_bytes());
            viewer.write(exit.as_bytes());
            assert_eq!(viewer.screen_rows(), source.screen_rows(), "mode {mode}, history {history}");
            if history > 0 {
                assert!(viewer.plain_text().contains("line 3"), "primary history must also be replayed");
            } else {
                assert!(!viewer.plain_text().contains("line 3"), "history zero must not replay scrollback");
            }
        }
    }
}

#[test]
fn the_primary_screens_pen_margins_and_charset_stay_out_of_the_alternate_replay() {
    let mut source = Terminal::new(30, 6, 1 << 20);
    // Red pen, a 2-4 scroll region and DEC line drawing on the primary screen.
    source.write(b"one\r\ntwo\r\n\x1b[31m\x1b[2;4r\x1b(0\x1b[?1049h\x1b[0m\x1b(B\x1b[r\x1b[Hqwerty\r\na\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng");
    let mut viewer = Terminal::new(30, 6, 1 << 20);
    viewer.write(&source.paint(0));
    assert_eq!(viewer.screen_rows(), source.screen_rows());
    assert_eq!(viewer.replay(), source.replay());
    for t in [&mut source, &mut viewer] {
        t.write(b"\x1b[?1049lx\r\ny\r\nz");
    }
    assert_eq!(viewer.screen_rows(), source.screen_rows());
    assert_eq!(viewer.replay(), source.replay());
}

#[test]
fn repaint_uses_the_active_screen_not_stale_alternate_screen_modes() {
    let mut t = Terminal::new(30, 4, 1 << 20);
    t.write(b"main\r\n\x1b[?47h\x1b[?1049hALT\x1b[?1049lDONE");
    assert!(!t.alt_screen());
    let mut fresh = Terminal::new(30, 4, 1 << 20);
    fresh.write(&t.paint(10));
    assert!(!fresh.alt_screen());
    assert_eq!(fresh.screen_rows(), t.screen_rows());

    for enter in [47, 1047, 1049] {
        for exit in [None, Some(47), Some(1047), Some(1049)] {
            let mut t = Terminal::new(30, 4, 1 << 20);
            t.write(format!("main\r\n\x1b[?{enter}h\x1b[2;3HALT").as_bytes());
            if let Some(exit) = exit {
                t.write(format!("\x1b[?{exit}lDONE").as_bytes());
            }
            let paint = t.paint(10);
            for viewer_mode in [None, Some(47), Some(1047), Some(1049)] {
                let mut viewer = Terminal::new(30, 4, 1 << 20);
                if let Some(mode) = viewer_mode {
                    viewer.write(format!("old main\x1b[?{mode}hOLD ALT").as_bytes());
                }
                viewer.write(&paint);
                let case = (enter, exit, viewer_mode);
                assert_eq!(viewer.alt_screen(), t.alt_screen(), "{case:?}");
                assert_eq!(viewer.screen_rows(), t.screen_rows(), "{case:?}");
                assert_eq!(viewer.dec_mode(1049), t.alt_screen(), "{case:?}");
            }
        }
    }
}

#[test]
fn program_status_reports_and_title_changes_reach_us() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    t.write(b"\x1b]7501;state=working:app=claude-code:progress=40\x1b\\");
    t.write(b"\x1b]7501;state=blocked:kind=permission:id=a/b:msg=QXBwbHk/\x07");
    t.write(b"\x1b]7501;state=clear\x1b\\\x1b]7501;state=bogus\x1b\\");
    let reports = t.take_status_reports();
    assert_eq!(reports.len(), 3, "{reports:?}");
    assert_eq!((reports[0].state, reports[0].progress, reports[0].app.as_str()), ("working", Some(40), "claude-code"));
    assert_eq!(
        (reports[1].kind, reports[1].id.as_str(), reports[1].message.as_str()),
        (Some("permission"), "a/b", "Apply?")
    );
    assert_eq!(reports[2].state, "clear");
    // A program asking whether we support the protocol gets an answer.
    t.write(b"\x1b]7501;?\x1b\\");
    assert!(String::from_utf8_lossy(&t.take_responses()).starts_with("\x1b]7501;?"));

    assert!(!t.take_title_changed());
    t.write(b"\x1b]2;hi\x07");
    assert!(t.take_title_changed());
}

#[test]
fn wheel_notches_follow_the_programs_mouse_mode_and_rows_keep_their_width() {
    let mut t = Terminal::new(20, 4, 1 << 20);
    assert!(t.wheel(true).is_empty(), "no mouse reports asked for");
    t.write(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h");
    assert_eq!(String::from_utf8_lossy(&t.wheel(true)), "\x1b[<64;11;3M");
    assert_eq!(String::from_utf8_lossy(&t.wheel(false)), "\x1b[<65;11;3M");
    t.write(b"hello\r\nworld  \x1b[2;19H\xe2\x94\x82");
    let rows = t.screen_rows();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], format!("{:<20}", "hello"));
    assert!(rows[1].starts_with("world") && rows[1].chars().nth(18) == Some('│'), "{:?}", rows[1]);
    assert!(rows.iter().all(|r| r.chars().count() == 20));
}

#[test]
fn answers_queries_with_viewer_facts() {
    let mut t = Terminal::new(80, 24, 1 << 20);
    t.set_xtversion("WezTerm 20240203");
    t.set_display(Some([1, 2, 3]), Some([0xff, 0xff, 0xff]), None, Some((9, 18)));
    let ask = |t: &mut Terminal, q: &[u8]| {
        t.write(q);
        String::from_utf8_lossy(&t.take_responses()).into_owned()
    };
    assert_eq!(ask(&mut t, b"\x1b[c"), "\x1b[?62;22c");
    assert_eq!(ask(&mut t, b"\x1b[>c"), "\x1b[>1;10;0c");
    assert!(ask(&mut t, b"\x1b[>q").contains("WezTerm 20240203"));
    assert_eq!(ask(&mut t, b"\x1b[3;5H\x1b[6n"), "\x1b[3;5R");
    assert!(ask(&mut t, b"\x1b]11;?\x07").contains("rgb:ffff/ffff/ffff"));
    assert!(ask(&mut t, b"\x1b]10;?\x07").contains("rgb:0101/0202/0303"));
    assert_eq!(ask(&mut t, b"\x1b[?996n"), "\x1b[?997;2n", "light background");
    assert_eq!(ask(&mut t, b"\x1b[16t"), "\x1b[6;18;9t");
    assert_eq!(ask(&mut t, b"\x1b[18t"), "\x1b[8;24;80t");
    assert_eq!(ask(&mut t, b"\x1b[?2004$p"), "\x1b[?2004;2$y");
    assert_eq!(ask(&mut t, b"\x1b[?u"), "\x1b[?0u");
}

#[test]
fn grapheme_clustering_is_on_from_the_start() {
    let mut t = Terminal::new(10, 2, 0);
    t.write(b"\x1b[?2027$p");
    assert_eq!(String::from_utf8_lossy(&t.take_responses()), "\x1b[?2027;1$y");
    t.write("\u{270d}\u{fe0f}!".as_bytes());
    assert_eq!(t.plain_text().trim_end(), "\u{270d}\u{fe0f}!");
    t.write(b"\x1bc");
    assert!(t.dec_mode(2027), "a reset keeps it on");
}

#[test]
fn a_replay_carries_the_text_and_colors_but_not_the_programs_modes_or_title() {
    let mut old = Terminal::new(80, 24, 1 << 20);
    old.write(b"\x1b[?1000h\x1b[?2004h\x1b]2;old title\x1b\\");
    for i in 0..30 {
        old.write(format!("line-{i}\r\n").as_bytes());
    }
    old.write(b"\x1b[38;2;255;0;0mred\x1b[0m\r\n\r\n");
    let replay = old.replay();
    assert!(String::from_utf8_lossy(&replay).contains("255;0;0"), "{:?}", String::from_utf8_lossy(&replay));
    let mut new = Terminal::new(80, 24, 1 << 20);
    new.write(&replay);
    let text = new.plain_text();
    assert!(text.contains("line-0") && text.contains("line-29") && text.trim_end().ends_with("red"), "{text:?}");
    assert!(!new.dec_mode(1000) && !new.dec_mode(2004) && new.title().is_empty());
}

#[test]
fn a_replay_of_a_full_screen_program_keeps_what_it_hid_then_its_last_frame() {
    let mut old = Terminal::new(80, 24, 1 << 20);
    old.write(b"$ agent\r\n\x1b[?1049h\x1b[Hlast-frame");
    let mut new = Terminal::new(80, 24, 1 << 20);
    new.write(&old.replay());
    let text = new.plain_text();
    assert!(
        !new.alt_screen() && text.find("$ agent") < text.find("last-frame") && text.contains("$ agent"),
        "{text:?}"
    );
}

#[test]
fn xtversion_is_rios_own_until_the_core_names_the_program() {
    let mut t = Terminal::new(80, 24, 0);
    t.write(b"\x1b[>q");
    assert!(String::from_utf8_lossy(&t.take_responses()).starts_with("\x1bP>|Rio "));
    // Terminfo questions are left for the viewer, which is the terminal the program really meets.
    t.write(b"\x1bP+q544e\x1b\\");
    assert!(t.take_responses().is_empty());
    t.set_xtversion("bty 1");
    t.write(b"\x1b[>0q\x1b[14t");
    assert_eq!(t.take_responses(), b"\x1bP>|bty 1\x1b\\\x1b[4;480;800t");
}

#[test]
fn queries_answer_in_the_order_they_were_asked() {
    let mut t = Terminal::new(80, 24, 0);
    t.write(b"\x1b]11;?\x1b\\\x1b[c\x1b[5n\x1b]4;1;?\x07\x1b[?996n\x1b[6n");
    assert_eq!(
        String::from_utf8_lossy(&t.take_responses()),
        "\x1b]11;rgb:2828/2c2c/3434\x1b\\\x1b[?62;22c\x1b[0n\x1b]4;1;rgb:cdcd/0000/0000\x07\x1b[?997;1n\x1b[1;1R"
    );
    // A color the program set is the one it reads back, and a snapshot keeps it.
    t.write(b"\x1b]4;1;rgb:12/34/56\x07");
    let mut back = Terminal::restore(&t.snapshot().unwrap(), 0).unwrap();
    back.write(b"\x1b]4;1;?\x07");
    assert_eq!(String::from_utf8_lossy(&back.take_responses()), "\x1b]4;1;rgb:1212/3434/5656\x07");
}

#[test]
fn a_soft_reset_restores_defaults_but_keeps_the_cursor_and_screen() {
    let mut t = Terminal::new(20, 5, 0);
    t.write(b"\x1b[2;4r\x1b[?25l\x1b[?7l\x1b[4h\x1b[1;31mhi\x1b[3;7H\x1b[!p");
    assert!(t.dec_mode(25) && t.dec_mode(7) && !t.term.mode().contains(Mode::INSERT));
    t.write(b"x");
    assert_eq!(t.screen_rows()[2], format!("{:<20}", "      x"));
    assert!(t.paint(0).windows(4).all(|w| w != b"\x1b[2;"), "margins are gone");
    assert!(!String::from_utf8_lossy(&t.replay()).contains("1;31mx"), "pen is reset");
}

#[test]
fn soft_wraps_hyperlinks_and_combining_marks_survive_a_repaint() {
    let mut t = Terminal::new(10, 4, 1 << 20);
    t.write("0123456789abc\r\n\x1b]8;id=x;https://e.x\x1b\\link\x1b]8;;\x1b\\ e\u{301}".as_bytes());
    let mut fresh = Terminal::new(10, 4, 1 << 20);
    fresh.write(&t.paint(0));
    assert_eq!(fresh.plain_text(), "0123456789abc\nlink e\u{301}");
    assert_eq!(fresh.screen_rows(), t.screen_rows());
    assert!(String::from_utf8_lossy(&t.paint(0)).contains("\x1b]8;id=x;https://e.x\x1b\\link"));
    // The viewer reflows the wrapped line the way the core does.
    t.resize(20, 4);
    fresh.resize(20, 4);
    assert_eq!(fresh.screen_rows(), t.screen_rows());
}

#[test]
fn a_cursor_waiting_to_wrap_still_waits_after_a_repaint() {
    let mut t = Terminal::new(5, 3, 0);
    t.write(b"abcde");
    let mut fresh = Terminal::new(5, 3, 0);
    fresh.write(&t.paint(0));
    for s in [&mut t, &mut fresh] {
        s.write(b"f");
    }
    assert_eq!(fresh.plain_text(), "abcdef");
    assert_eq!(fresh.screen_rows(), t.screen_rows());
}

#[test]
fn a_snapshot_on_the_alternate_screen_brings_back_both_screens() {
    let mut t = Terminal::new(20, 4, 1 << 20);
    t.write(b"\x1b]2;t\x07\x1b[?2004h");
    for i in 0..6 {
        t.write(format!("old {i}\r\n").as_bytes());
    }
    t.write(b"\x1b[?1049h\x1b[?1000hfull");
    let mut back = Terminal::restore(&t.snapshot().unwrap(), 1 << 20).unwrap();
    assert!(back.alt_screen() && back.dec_mode(1000) && back.dec_mode(2004));
    assert_eq!((back.screen_rows(), back.title()), (t.screen_rows(), t.title()));
    for s in [&mut t, &mut back] {
        s.write(b"\x1b[?1049l");
    }
    assert_eq!(back.plain_text(), t.plain_text());
    assert!(back.plain_text().contains("old 0"));
}

#[test]
fn a_resize_on_the_alternate_screen_reflows_the_hidden_primary_too() {
    let mut t = Terminal::new(10, 4, 1 << 20);
    t.write(b"0123456789abcdef\r\n\x1b[?1049hfull");
    t.resize(20, 4);
    let mut fresh = Terminal::new(20, 4, 1 << 20);
    fresh.write(&t.paint(10));
    for s in [&mut t, &mut fresh] {
        s.write(b"\x1b[?1049l");
    }
    assert_eq!(fresh.screen_rows(), t.screen_rows());
    assert!(t.plain_text().contains("0123456789abcdef"), "{:?}", t.plain_text());
}

#[test]
fn keys_follow_the_programs_keyboard_modes() {
    let mut t = Terminal::new(80, 24, 0);
    let key = |t: &Terminal, k, m, s| String::from_utf8(t.encode_key(k, m, s)).unwrap();
    assert_eq!(key(&t, Key::Up, 0, ""), "\x1b[A");
    assert_eq!(key(&t, Key::Up, CTRL | SHIFT, ""), "\x1b[1;6A");
    assert_eq!(key(&t, Key::F(5), 0, ""), "\x1b[15~");
    assert_eq!(key(&t, Key::Tab, SHIFT, ""), "\x1b[Z");
    assert_eq!(key(&t, Key::Char('x'), ALT, ""), "\x1bx");
    assert_eq!(key(&t, Key::Char('a'), SUPER, ""), "");
    t.write(b"\x1b[>4;2m");
    assert_eq!(key(&t, Key::Char('c'), CTRL, ""), "\x1b[27;5;99~");
    t.write(b"\x1b[>4m\x1b[=9;1u");
    assert_eq!(key(&t, Key::Char('a'), SHIFT, "A"), "\x1b[97;2u");
    assert_eq!(key(&t, Key::Enter, 0, ""), "\x1b[13u");
}

/// The scanner must find the same sequence ends as rio's parser: a cell is
/// only drawn at ground, and each case ends in ground before its last `A`.
#[test]
fn sequence_ends_agree_with_rios_parser() {
    let cases: &[&[u8]] = &[
        b"\x1b[31mA",
        b"\x1b[?1;2$pA",
        b"\x1b[3\n1mA",
        b"\x1b(0qA",
        b"\x1b#8A",
        b"\x1b]0;t\x07A",
        b"\x1b]0;t\x1b\\A",
        b"\x1bP1$r\x9cA",
        b"\x1bPq#0\x07A\x1b\\A",
        b"\x1b_Gx\x07A",
        b"\x1b[\x1b[mA",
        b"\x1b[1\x18A",
        b"\x1b]2;\x1aA",
        b"\x1b\x1b[mA",
        b"\x1b $\x1b[mA",
        "\u{4e0a}A".as_bytes(),
        b"\xe4\xb8\x1b[mA",
        b"\xe0\x80A",
        b"\x1b[?>1hA",
    ];
    for &case in cases {
        let mut t = Terminal::new(40, 3, 0);
        let n = t.scan.until_ground(&case[1..]).map(|n| n + 1);
        for (i, &b) in case.iter().enumerate() {
            t.write(&[b]);
            let at_ground = t.continuation().is_some_and(|c| c.is_empty());
            let drawn = t.screen_rows().iter().any(|r| r.contains('A'));
            if drawn {
                assert!(at_ground, "{case:?}: drew A at {i} outside ground");
            }
            if i + 2 == case.len() {
                assert_eq!(t.continuation(), Some(Vec::new()), "{case:?}: not at ground before A");
            }
        }
        assert!(t.screen_rows().iter().any(|r| r.contains('A')), "{case:?}: {:?}", t.screen_rows());
        assert!(n.is_some(), "{case:?}");
    }
}

#[test]
fn clearing_a_blank_screen_adds_nothing_to_scrollback() {
    let mut t = Terminal::new(10, 3, 1 << 20);
    t.write(b"\x1b[2;2H\x1b[2J\x1b[2Jx");
    assert_eq!(t.plain_text(), "\n x");
    t.write(b"\x1b[2J");
    assert_eq!(t.plain_text(), "\n x", "a screen with content still goes into scrollback");
    assert_eq!(t.screen_rows()[1], format!("{:<10}", ""));
}
