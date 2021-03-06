from __future__ import absolute_import, print_function

import os

from edenscm.mercurial import dispatch
from hghave import require


require(["py2"])


def testdispatch(cmd):
    """Simple wrapper around dispatch.dispatch()

    Prints command and result value, but does not handle quoting.
    """
    print("running: %s" % (cmd,))
    req = dispatch.request(cmd.split())
    result = dispatch.dispatch(req)
    print("result: %r" % (result,))


testdispatch("init test1")
os.chdir("test1")

# create file 'foo', add and commit
f = open("foo", "wb")
f.write(b"foo\n")
f.close()
testdispatch("add foo")
testdispatch("commit -m commit1 -d 2000-01-01 foo")

# append to file 'foo' and commit
f = open("foo", "ab")
f.write(b"bar\n")
f.close()
testdispatch("commit -m commit2 -d 2000-01-02 foo")

# check 88803a69b24 (fancyopts modified command table)
testdispatch("log -r 0")
testdispatch("log -r tip")
