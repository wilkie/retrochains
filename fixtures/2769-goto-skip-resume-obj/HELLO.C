int select(int flag) {
  int s;
  s = 0;
  if (flag == 0) goto skip;
  s = s + 10;
skip:
  s = s + 1;
  return s;
}
