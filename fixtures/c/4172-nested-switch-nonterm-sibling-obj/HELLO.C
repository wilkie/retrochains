int act(int x, int y) {
  switch (x) {
    case 1:
      switch (y) { case 1: return 10; case 2: return 20; }
      break;
    case 2:
      y = y + 1;
      break;
  }
  return y;
}
