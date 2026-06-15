char *words[] = {"foo", "bar", "baz", "qux"};
int main(void) {
  int i;
  int sum;
  sum = 0;
  for (i = 0; i < 4; i = i + 1) {
    sum = sum + words[i][0];
  }
  return sum;
}
