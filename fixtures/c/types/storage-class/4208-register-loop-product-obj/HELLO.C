int main(void)
{
  register int p;
  int i;

  p = 1;
  for (i = 1; i <= 5; i++)
    p = p * i;
  return p;
}
