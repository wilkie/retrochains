int act(int x) {
  switch (x) {
    case 1:
    case 2:
    case 3:
      return 100;
    case 4:
      return 200;
  }
  return -1;
}
