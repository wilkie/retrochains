int main(void) {
  int x = 2;
  int r;
  switch (x) {
    case 1:
    case 2:
    case 3:
      r = 100;
      break;
    default:
      r = 0;
      break;
  }
  return r;
}
