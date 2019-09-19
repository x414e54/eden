  $ . "${TEST_FIXTURES}/library.sh"

setup configuration

  $ REPOTYPE="blob:files"
  $ setup_common_config $REPOTYPE
  $ setup_commitsyncmap

  $ cd $TESTTMP

setup hg server repo

  $ hginit_treemanifest repo-hg
  $ cd repo-hg
  $ function createfile { mkdir -p "$(dirname  $1)" && echo "$1" > "$1" && hg add "$1"; }

-- create some semblance of fbsource
  $ createfile fbcode/fbcodfile_fbsource
  $ createfile fbobjc/fbobjcfile_fbsource
  $ createfile fbandroid/fbandroidfile_fbsource
  $ createfile xplat/xplatfile_fbsource
  $ createfile arvr/arvrfile_fbsource
  $ createfile third-party/thirdpartyfile_fbsource
  $ hg ci -m "fbsource-like commit"
  $ hg book -r . fbsourcemaster

-- create some semblance of ovrsource
  $ hg up null -q
  $ createfile fbcode/fbcodfile_ovrsource
  $ createfile fbobjc/fbobjcfile_ovrsource
  $ createfile fbandroid/fbandroidfile_ovrsource
  $ createfile xplat/xplatfile_ovrsource
  $ createfile arvr/arvrfile_ovrsource
  $ createfile third-party/thirdpartyfile_ovrsource
  $ createfile Software/softwarefile_ovrsource
  $ createfile Research/researchfile_ovrsource
  $ hg ci -m "ovrsource-like commit"
  $ hg book -r . ovrsourcemaster

  $ hg log -T "{node} {bookmarks}\n" -r "all()"
  4da689e6447cf99bbc121eaa7b05ea1504cf2f7c fbsourcemaster
  4d79e7d65a781c6c80b3ee4faf63452e8beafa97 ovrsourcemaster

  $ cd $TESTTMP

setup repo-pull
  $ hgclone_treemanifest ssh://user@dummy/repo-hg repo-pull --noupdate

blobimport
  $ blobimport repo-hg/.hg repo

  $ export COMMIT_DATE="1985-04-12T23:20:50.52Z"
move things in fbsource
  $ megarepo_tool move 1 4da689e6447cf99bbc121eaa7b05ea1504cf2f7c user "fbsource move" --mark-public --commit-date-rfc3339 "$COMMIT_DATE"
  * using repo "repo" repoid RepositoryId(0) (glob)
  * changset resolved as: * (glob)
  * Requesting the hg changeset (glob)
  * Hg changeset: HgChangesetId(HgNodeHash(Sha1(2d1c2ac8acbc245768933d05d23bd248cf6a16bb))) (glob)
  * Marking changeset as public (glob)
  * Done marking as public (glob)

move things in ovrsource
  $ megarepo_tool move 2 4d79e7d65a781c6c80b3ee4faf63452e8beafa97 user "ovrsource move" --mark-public --commit-date-rfc3339 "$COMMIT_DATE"
  * using repo "repo" repoid RepositoryId(0) (glob)
  * changset resolved as: * (glob)
  * Requesting the hg changeset (glob)
  * Hg changeset: HgChangesetId(HgNodeHash(Sha1(3c5f72c6d1ed24bd0914ff5ecd96b98f216002b4))) (glob)
  * Marking changeset as public (glob)
  * Done marking as public (glob)

merge things in both repos
  $ megarepo_tool merge 2d1c2ac8acbc245768933d05d23bd248cf6a16bb 3c5f72c6d1ed24bd0914ff5ecd96b98f216002b4 user "megarepo merge" --mark-public --commit-date-rfc3339 "$COMMIT_DATE"
  * using repo "repo" repoid RepositoryId(0) (glob)
  * Creating a merge commit (glob)
  * Checking if there are any path conflicts (glob)
  * Done checking path conflicts (glob)
  * Creating a merge bonsai changeset with parents: * (glob)
  * Marked as public * (glob)
  * Created *. Generating an HG equivalent (glob)
  * Hg changeset: HgChangesetId(HgNodeHash(Sha1(0383059155f1a61cdba994a8bb686c15f378794b))) (glob)
  $ mononoke_admin bookmarks set master 0383059155f1a61cdba994a8bb686c15f378794b
  * using repo "repo" repoid RepositoryId(0) (glob)

start mononoke server
  $ mononoke
  $ wait_for_mononoke "$TESTTMP/repo"

pull the result
  $ cd $TESTTMP/repo-pull
  $ hgmn -q pull && hgmn -q up master
  $ ls -1
  arvr
  arvr-legacy
  fbandroid
  fbcode
  fbobjc
  third-party
  xplat
  $ ls -1 fbcode fbandroid fbobjc xplat arvr arvr-legacy
  arvr:
  arvrfile_ovrsource
  
  arvr-legacy:
  Research
  Software
  third-party
  
  fbandroid:
  fbandroidfile_fbsource
  
  fbcode:
  fbcodfile_fbsource
  
  fbobjc:
  fbobjcfile_fbsource
  
  xplat:
  xplatfile_fbsource
