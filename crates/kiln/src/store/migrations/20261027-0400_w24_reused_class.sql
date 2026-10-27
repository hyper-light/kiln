-- W24-R30, R28: a leg reused through an equivalence class names the class it was found in.
ALTER TABLE jobs ADD COLUMN reused_class TEXT;
