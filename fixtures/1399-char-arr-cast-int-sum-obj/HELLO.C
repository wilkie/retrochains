char a[4] = {1, 2, 3, 4};
int main(void) {
  int s = 0;
  int i;
  for (i = 0; i < 4; i++) s += (int)a[i];
  return s;
}
