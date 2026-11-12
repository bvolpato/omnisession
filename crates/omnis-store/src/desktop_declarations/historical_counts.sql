CREATE TEMP TABLE historical_desktop_counts AS
WITH declarations(provider,session_id,source_home,source_file,steps) AS (
    SELECT 'antigravity-desktop',session_id,source_home,source_file,steps FROM desktop_step_declarations
    UNION ALL
    SELECT 'antigravity-ide',session_id,source_home,source_file,steps FROM ide_step_declarations
), receipts AS (
    SELECT provider,session_id,source_home,source_file,source_contents,content_hash,length(step_proof) AS proof_bytes
    FROM desktop_search_receipts
    UNION ALL
    SELECT provider,session_id,source_home,source_file,source_contents,content_hash,length(step_proof)
    FROM antigravity_ide_search_receipts
), observed AS (
    SELECT r.provider,r.session_id,r.source_home,r.source_file,c.event_count AS steps,
           d.indexed_at AS observed_at,d.indexed_at AS latest,declared.steps IS NULL AS inferred,
           json_object('title',c.title,'project_path',c.project_path,'git_branch',c.git_branch,
                       'created_at',c.created_at,'updated_at',c.updated_at,'approximate',c.updated_at_approximate) AS summary
    FROM receipts r
    JOIN session_trajectories d USING(provider,session_id)
    JOIN native_search_document_homes o ON o.trajectory_id=d.id
    JOIN session_index c USING(provider,session_id)
    JOIN native_catalog_homes h USING(provider,session_id)
    LEFT JOIN declarations declared USING(provider,session_id,source_home,source_file)
    WHERE d.origin='native' AND o.verification=1 AND h.verified
      AND r.source_home=o.source_home AND r.source_home=h.home
      AND r.source_file=o.source_file AND r.source_file=d.source_file_id AND r.source_file=c.source_file_id
      AND r.source_contents=d.source_contents_id AND r.source_contents=c.source_contents_id
      AND r.content_hash=d.content_hash
      AND (declared.steps=c.event_count OR (
          declared.steps IS NULL AND r.provider='antigravity-ide' AND (d.source_complete OR d.document_version=9)
          AND c.event_count>0 AND r.proof_bytes>0 AND NOT d.provider_limited))
      AND (r.proof_bytes=16*c.event_count OR
          (NOT d.source_complete AND instr(d.truncation_strategy,'source_incomplete')>0
           AND r.proof_bytes<16*c.event_count))

    UNION ALL
    SELECT r.provider,r.session_id,r.source_home,r.source_file,r.proof_bytes/16 AS steps,
           NULL AS observed_at,primary_doc.indexed_at AS latest,declared.steps IS NULL AS inferred,NULL AS summary
    FROM receipts r
    JOIN scoped_native_documents d USING(provider,session_id,source_home,source_file,source_contents)
    JOIN session_trajectories primary_doc USING(provider,session_id)
    LEFT JOIN declarations declared USING(provider,session_id,source_home,source_file)
    WHERE d.source_complete AND NOT d.provider_limited AND omnis_receipt_matches_text(d.stored_text,r.content_hash)
      AND (declared.steps=r.proof_bytes/16 OR
          (declared.steps IS NULL AND r.provider='antigravity-ide' AND d.document_version=9 AND r.proof_bytes>0))
      AND NOT EXISTS (SELECT 1 FROM native_search_document_homes o WHERE o.trajectory_id=primary_doc.id
          AND o.verification=1 AND o.source_home=r.source_home AND o.source_file=r.source_file)
)
SELECT o.* FROM observed o
WHERE NOT EXISTS (
    SELECT 1 FROM declarations a
    WHERE a.session_id=o.session_id AND a.source_home=o.source_home AND a.source_file=o.source_file
      AND a.steps<>o.steps AND NOT EXISTS (
          SELECT 1 FROM observed b WHERE b.provider=a.provider AND b.session_id=a.session_id
            AND b.source_home=a.source_home AND b.source_file=a.source_file AND b.steps=a.steps));

CREATE TEMP TABLE recovered_desktop_counts AS
SELECT o.* FROM historical_desktop_counts o
LEFT JOIN physical_step_declarations current USING(session_id,source_home,source_file)
WHERE current.source_contents IS NULL AND (current.generation IS NULL OR current.steps=o.steps OR o.inferred)
  AND NOT EXISTS (
      SELECT 1 FROM historical_desktop_counts other WHERE other.session_id=o.session_id
        AND other.source_home=o.source_home AND other.source_file=o.source_file AND other.steps<>o.steps
        AND (o.observed_at IS NULL OR o.observed_at<=other.latest));

CREATE TEMP TABLE unranked_desktop_counts AS
SELECT a.session_id,a.source_home,a.source_file
FROM historical_desktop_counts a
LEFT JOIN physical_step_declarations current USING(session_id,source_home,source_file)
WHERE current.source_contents IS NULL AND NOT EXISTS (
    SELECT 1 FROM recovered_desktop_counts winner WHERE winner.session_id=a.session_id
      AND winner.source_home=a.source_home AND winner.source_file=a.source_file)
GROUP BY a.session_id,a.source_home,a.source_file
HAVING min(a.steps)<>max(a.steps) AND (max(current.generation) IS NULL OR max(a.inferred));

INSERT OR IGNORE INTO ide_step_declarations(session_id,source_home,source_file,steps,summary)
SELECT session_id,source_home,source_file,steps,summary FROM recovered_desktop_counts WHERE inferred
UNION ALL SELECT o.session_id,o.source_home,o.source_file,o.steps,o.summary
FROM historical_desktop_counts o JOIN unranked_desktop_counts USING(session_id,source_home,source_file)
WHERE o.inferred;

INSERT INTO physical_step_declarations(session_id,source_home,source_file,steps)
SELECT DISTINCT session_id,source_home,source_file,steps FROM recovered_desktop_counts WHERE true
ON CONFLICT(session_id,source_home,source_file) DO UPDATE SET steps=excluded.steps
WHERE physical_step_declarations.source_contents IS NULL AND physical_step_declarations.steps<>excluded.steps;

DELETE FROM physical_step_declarations WHERE (session_id,source_home,source_file) IN
    (SELECT session_id,source_home,source_file FROM unranked_desktop_counts);

DROP TABLE unranked_desktop_counts;
DROP TABLE recovered_desktop_counts;
DROP TABLE historical_desktop_counts;
