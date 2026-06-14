int act(int x, int y) {
  switch (x) {
    case 1:
      switch (y) { case 1: y = 100; break; case 2: y = 200; break; }
      break;
    case 2:
      y = y + 5;
      break;
    case 3:
      return 30;
  }
  return y;
}
