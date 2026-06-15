int main(void) {
  char a[3];
  a[0] = 'X';
  a[1] = 'Y';
  a[2] = 'X';
  if (a[0] == a[2]) return 1;
  return 0;
}
