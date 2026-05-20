char buf[5];
int main(void) {
  int i;
  for (i = 0; i < 5; i++) buf[i] = 'X';
  return buf[2];
}
