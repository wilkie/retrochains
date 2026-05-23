int classify(int x) {
  switch (x) {
    case 1:
    case 2:
    case 3:
      return 100;
    case 5:
    case 7:
      return 200;
    default:
      return 0;
  }
}
