int main(void) {
  char buf[4];
  char *cp = buf;
  int *ip = (int *)buf;
  buf[0] = 'A';
  buf[1] = 'B';
  buf[2] = 'C';
  buf[3] = 'D';
  cp += 1;
  ip += 1;
  return cp[0] + *ip;
}
