int main(void) {
  int i;
  int j;
  int s = 0;
  for (i = 0; i < 3; i++)
    for (j = 0; j < 2; j++)
      s += i * j;
  return s;
}
